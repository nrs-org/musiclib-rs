//! PoC: can TypeSafe's `jev-latest` System One model do soft-dedup identity
//! judgments? Pulls the real human-labeled pairs out of the `dedup_feedback`
//! table (recorded from the player UI's link/unlink actions), builds the same
//! `EntryInfo` the Rhai/learned-model scorers already use, strips it down to
//! content signals only (titles/aliases/durations/dates/types/credited-artist
//! names -- no raw source identifiers, so the model can't just ID-match), and
//! asks Jev a batched Choice+Noul question per pair. Compares Jev's `identity`
//! choice against the human label and reports token usage/cost.
//!
//! Without `TYPESAFE_API_KEY` set, runs in dry-run mode: prints the request
//! bodies it would send instead of calling the API.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context as _;
use clap::Parser;
use futures::{StreamExt, stream};
use musiclib_rs::{
    app_dirs,
    musicdb::{DedupFeedbackRow, MusicDb},
    pipeline::softmatch::{EntryInfo, Verdict, entry_infos_by_ids},
};
use rusqlite::Connection as SqliteConnection;
use serde::{Deserialize, Serialize};

const API_URL: &str = "https://api.typesafe.ai/v1/systemone";
const MODEL: &str = "jev-latest";
/// From typesafe.ai's published pricing: $42 / 1B input tokens, output free.
const INPUT_USD_PER_TOKEN: f64 = 0.042 / 1_000_000.0;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "PoC: score real dedup_feedback pairs with TypeSafe's Jev model"
)]
struct Args {
    /// Music library database. Defaults to <data_dir>/musiclib.db. Opened read-only.
    #[arg(long)]
    db: Option<String>,
    /// Only test the first N labeled pairs (0 = all).
    #[arg(long, default_value_t = 0)]
    limit: usize,
    /// Only test these specific pairs, e.g. "31-283,192-337". Overrides --limit.
    #[arg(long)]
    pairs: Option<String>,
    /// Print full entry evidence and every answer's probabilities/confidence,
    /// not just the `identity` choice.
    #[arg(long)]
    verbose: bool,
    /// Force dry-run (print request bodies, no network calls) even if
    /// TYPESAFE_API_KEY is set.
    #[arg(long)]
    dry_run: bool,
    /// CSV file with header `entry_a,entry_b,label,heuristic_confidence` --
    /// switches to relatedness-eval mode instead of the dedup_feedback flow.
    /// `label` is a free string (e.g. "positive"/"negative");
    /// `heuristic_confidence` may be blank.
    #[arg(long)]
    pairs_file: Option<String>,
    /// Max concurrent API calls in relatedness-eval mode.
    #[arg(long, default_value_t = 12)]
    concurrency: usize,
    /// Which question set --pairs-file uses: "related" (the eval question) or
    /// "verdict" (the design sketch mirroring pipeline::softmatch::Verdict --
    /// merge/relate_variant/distinct/defer). Not wired into any real
    /// decision-making; see build_verdict_questions' doc comment.
    #[arg(long, default_value = "related")]
    questions: String,
    /// Genuine should-MERGE eval: for entries already carrying pairs from >=2
    /// distinct sources, split those pairs into two per-source "halves" and
    /// ask whether they're the same identity -- ground truth is always yes,
    /// since both halves are the same real entry_id today. Matched with
    /// random-distinct-entry negatives built the same restricted way. See
    /// `run_merge_eval`'s doc comment.
    #[arg(long)]
    merge_eval: bool,
    /// Entries sampled per type in --merge-eval mode (for each of positives
    /// and negatives).
    #[arg(long, default_value_t = 60)]
    merge_eval_n: usize,
}

#[derive(Debug, Clone, Serialize)]
struct TitledAlias {
    source: String,
    name: String,
}

#[derive(Debug, Clone, Serialize)]
struct TrackPosition {
    /// Title of the release entry this position is on -- lets Jev tell
    /// "same disc/track slot on the same release" from "same slot, different
    /// release" without seeing raw entry ids.
    release_title: Option<String>,
    disc_no: Option<i32>,
    track_no: Option<i32>,
}

#[derive(Debug, Clone, Serialize)]
struct EntryView {
    entry_type: String,
    /// Top sourced aliases (clean sources before video-description sources,
    /// per the canonical ordering `EntryInfo` already keeps), each tagged
    /// with which source it came from.
    titles: Vec<TitledAlias>,
    durations_sec: Vec<f64>,
    release_dates: Vec<String>,
    release_types: Vec<String>,
    primary_types: Vec<String>,
    /// Which source namespaces this entry is known from (e.g. "youtube",
    /// "spotify", "musicbrainz") -- not raw identifiers, just platform
    /// breadth/overlap as a weak corroborating signal.
    sources: Vec<String>,
    /// Resolved names of credited artists (for tracks/releases) or credited
    /// tracks (for artists) -- raw peer entry ids are meaningless to the model.
    credited_names: Vec<String>,
    /// For tracks: which release(s) it appears on and at what disc/track slot.
    track_positions: Vec<TrackPosition>,
    /// For releases: titles of the tracks it contains. `child_entry_ids` is
    /// overloaded on the Spotify/MusicBrainz backends -- for a track entry it
    /// also carries credited-artist and parent-album ids via the same
    /// `entry_child` edge type, so this filters to entries whose own
    /// `entry_type` is "track" (a release's real children; empty and
    /// redundant-by-design for a track entry, since credited_names/
    /// track_positions already cover that ground).
    release_tracks: Vec<String>,
    /// Human-readable handles/slugs pulled from source identifiers (e.g. a
    /// YouTube `@handle`, a Twitter/Genius/Marshmallow username) -- NOT the
    /// opaque per-platform IDs (channel IDs, UUIDs, base62 hashes, numeric
    /// catalog IDs) that `sources` deliberately omits. A handle is content
    /// (a name choice a human made), unlike an opaque ID, so exposing it
    /// doesn't let the model trivially ID-match; it's evidence of the same
    /// kind as a title alias.
    handles: Vec<String>,
    /// For releases: internal entry ids of the release_group(s) this release
    /// belongs to (via `entry_child`, release_group as parent). NOT an
    /// external provider id -- it's this DB's own structural edge, and its
    /// only use is exact-match comparison against the other side's list, so
    /// exposing it doesn't create an ID-shortcut the way a raw source
    /// identifier would. The catalog (MusicBrainz/Discogs) deliberately
    /// splits a release_group into multiple release entities for different
    /// editions/pressings -- so two releases sharing a release_group id are
    /// most likely intentionally-different editions, not a duplicate
    /// catalogued twice, which is close to the opposite of what tracklist
    /// overlap alone would suggest.
    release_group_ids: Vec<i64>,
}

const TOP_TITLES: usize = 8;
const CREDITED_NAMES_CAP: usize = 8;
const CHILD_TRACKS_CAP: usize = 12;
const TRACK_POSITIONS_CAP: usize = 5;
const HANDLES_CAP: usize = 6;

/// Pull a human-readable slug out of a source identifier (URL), or `None` if
/// the trailing path segment looks like an opaque platform ID (numeric
/// catalog ID, UUID, YouTube channel ID, base62 hash) rather than a name a
/// human actually chose.
fn extract_handle(identifier: &str) -> Option<String> {
    let after_scheme = identifier.splitn(2, "://").nth(1)?;
    let without_query = after_scheme
        .split(['?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let core = without_query.split('/').filter(|s| !s.is_empty()).last()?;
    let core = core.trim_start_matches('@');
    if core.is_empty() {
        return None;
    }
    let is_all_digit = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    if is_all_digit(core) || is_all_digit(core.strip_prefix("id").unwrap_or(core)) {
        return None; // apple/deezer/discogs/bilibili/etc numeric catalog id
    }
    if core.len() == 24
        && core.starts_with("UC")
        && core
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None; // youtube channel id
    }
    if core.len() == 36 && core.matches('-').count() == 4 {
        return None; // musicbrainz uuid
    }
    if core.len() >= 20 && core.chars().all(|c| c.is_ascii_hexdigit()) {
        return None; // hex hash (e.g. an archived image url's content hash)
    }
    {
        let mut chars = core.chars();
        if let Some(first) = chars.next() {
            if core.len() >= 5
                && first.is_ascii_uppercase()
                && chars.clone().all(|c| c.is_ascii_digit())
            {
                return None; // single-letter-prefixed numeric id (ASIN, wikidata Q-id, ...)
            }
        }
    }
    let has_digit = core.chars().any(|c| c.is_ascii_digit());
    let has_upper = core.chars().any(|c| c.is_ascii_uppercase());
    let has_lower = core.chars().any(|c| c.is_ascii_lowercase());
    if core.len() >= 15 && has_digit && has_upper && has_lower {
        return None; // spotify/discogs-style base62 opaque id
    }
    let decoded = urlencoding::decode(core)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| core.to_string());
    if decoded.is_empty() {
        None
    } else {
        Some(decoded)
    }
}

fn resolve_title(all: &HashMap<i64, EntryInfo>, id: i64) -> Option<String> {
    all.get(&id)
        .and_then(|e| e.best_title.clone().or_else(|| e.aliases.first().cloned()))
}

/// Entry ids of any release_group entries that `entry_id` is linked to as a
/// child (via `entry_child`, release_group as parent) -- a release's
/// membership in a release_group is exactly the "different pressing/edition
/// of the same overarching work" structural signal MusicBrainz/Discogs
/// encode by deliberately keeping releases under one group as separate
/// entities, so this is the opposite of "duplicate catalogued twice".
fn release_group_ids_for(conn: &SqliteConnection, entry_id: i64) -> anyhow::Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT es2.entry_id FROM entry_source es1 \
         JOIN entry_child ec ON ec.child_source = es1.source AND ec.child_identifier = es1.identifier \
         JOIN entry_source es2 ON es2.source = ec.parent_source AND es2.identifier = ec.parent_identifier \
         JOIN entry e ON e.id = es2.entry_id \
         WHERE es1.entry_id = ?1 AND e.entry_type = 'release_group'",
    )?;
    let ids = stmt
        .query_map(rusqlite::params![entry_id], |row| row.get(0))?
        .collect::<Result<Vec<i64>, _>>()?;
    Ok(ids)
}

/// Batch version of `release_group_ids_for` for every id in `entry_ids`,
/// opening one short-lived read-only connection.
fn load_release_groups(
    db_path: &std::path::Path,
    entry_ids: &[i64],
) -> anyhow::Result<HashMap<i64, Vec<i64>>> {
    let conn =
        SqliteConnection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening {} read-only via rusqlite", db_path.display()))?;
    let mut out = HashMap::new();
    for &id in entry_ids {
        let groups = release_group_ids_for(&conn, id)?;
        if !groups.is_empty() {
            out.insert(id, groups);
        }
    }
    Ok(out)
}

fn entry_view(
    e: &EntryInfo,
    all: &HashMap<i64, EntryInfo>,
    release_groups: &HashMap<i64, Vec<i64>>,
) -> EntryView {
    let mut credited_names: Vec<String> = e
        .peer_entry_ids
        .iter()
        .filter_map(|&id| resolve_title(all, id))
        .collect();
    credited_names.sort();
    credited_names.dedup();
    credited_names.truncate(CREDITED_NAMES_CAP);

    let titles: Vec<TitledAlias> = e
        .sourced_aliases
        .iter()
        .take(TOP_TITLES)
        .map(|(src, name, _primary)| TitledAlias {
            source: src.clone(),
            name: name.clone(),
        })
        .collect();

    let mut sources: Vec<String> = e.pairs.iter().map(|(src, _)| src.clone()).collect();
    sources.sort();
    sources.dedup();

    let mut handles: Vec<String> = e
        .pairs
        .iter()
        .filter_map(|(_, identifier)| extract_handle(identifier))
        .collect();
    handles.sort();
    handles.dedup();
    handles.truncate(HANDLES_CAP);

    let mut position_keys: Vec<(i64, Option<i32>, Option<i32>)> =
        e.track_positions.iter().copied().collect();
    position_keys.sort_unstable();
    position_keys.dedup();
    let track_positions: Vec<TrackPosition> = position_keys
        .into_iter()
        .take(TRACK_POSITIONS_CAP)
        .map(|(release_id, disc_no, track_no)| TrackPosition {
            release_title: resolve_title(all, release_id),
            disc_no,
            track_no,
        })
        .collect();

    let mut release_tracks: Vec<String> = e
        .child_entry_ids
        .iter()
        .filter(|&&id| all.get(&id).is_some_and(|c| c.entry_type == "track"))
        .filter_map(|&id| resolve_title(all, id))
        .collect();
    release_tracks.sort();
    release_tracks.dedup();
    release_tracks.truncate(CHILD_TRACKS_CAP);

    EntryView {
        entry_type: e.entry_type.clone(),
        titles,
        durations_sec: e.durations.iter().map(|ms| *ms as f64 / 1000.0).collect(),
        release_dates: e.release_dates.clone(),
        release_types: e.release_types.clone(),
        primary_types: e.primary_types.clone(),
        sources,
        credited_names,
        track_positions,
        release_tracks,
        handles,
        release_group_ids: release_groups.get(&e.entry_id).cloned().unwrap_or_default(),
    }
}

#[derive(Serialize)]
struct StatePayload {
    a: EntryView,
    b: EntryView,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Question {
    Noul {
        instructions: String,
    },
    Choice {
        instructions: String,
        criteria: BTreeMap<String, String>,
    },
}

#[derive(Serialize)]
struct TypesafeRequest {
    state: StatePayload,
    model: String,
    questions: BTreeMap<String, Question>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
#[allow(dead_code)]
enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    Score {
        score: f64,
        #[serde(default)]
        legend: BTreeMap<String, String>,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
}

#[derive(Debug, Deserialize)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct TypesafeResponse {
    answers: BTreeMap<String, Answer>,
    usage: Usage,
}

/// `criteria` is the type-specific set of non-`unsure` choices (each type
/// picks its own vocabulary -- e.g. track's 3-way same_identity/
/// related_variant/unrelated vs. artist's 2-way same_identity/
/// different_identity); `unsure` is appended automatically since its meaning
/// never varies by type.
fn identity_choice(instructions: &str, criteria: &[(&str, &str)]) -> Question {
    let mut map: BTreeMap<String, String> = criteria
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    map.insert(
        "unsure".to_string(),
        "the evidence is genuinely insufficient to decide either way.".to_string(),
    );
    Question::Choice {
        instructions: instructions.to_string(),
        criteria: map,
    }
}

/// `entry_type` selects a dedicated `identity` framing per type -- each has a
/// different notion of what "the same entity" means and a different set of
/// pitfalls, so one generic wording reused across all four systematically
/// under- or over-fires depending on type (confirmed empirically: splitting
/// out just the artist wording took MERGE/artist agreement from 47.4% to
/// 89.5%, see [[typesafe-soft-dedup-poc-findings]] in project memory).
fn build_questions(entry_type: &str) -> BTreeMap<String, Question> {
    let mut q = BTreeMap::new();
    let identity_question = match entry_type {
        "artist" => identity_choice(
            "`a`/`b` are music-artist records aggregated from multiple databases (YouTube, \
                Spotify, MusicBrainz, Discogs, etc.). Are they the SAME real-world person, \
                group, or act? Language, romanization, capitalization, or channel-naming \
                differences (e.g. a YouTube auto \"<name> - Topic\" channel) are NOT evidence of \
                difference. A shared handle/username/official-link between the two records IS \
                strong evidence for same_identity even when display names differ. Use \
                different_identity when one is an individual member and the other the \
                group/duo/unit they belong to (never merge a member with their group, even if \
                closely associated), or when the shared name is generic/common with no other \
                corroborating overlap (handle, associated work, official link).",
            &[
                (
                    "same_identity",
                    "same real-world person/group/act (different script/romanization/\
                     capitalization/channel-naming counts as this) -- merge.",
                ),
                (
                    "different_identity",
                    "different entities -- e.g. a group vs. one of its members, or unrelated \
                     people/acts that merely share a name.",
                ),
            ],
        ),
        "release" => identity_choice(
            "`a`/`b` are release (album/EP/single/compilation) records from multiple databases, \
                each listing `release_tracks`. Substantial tracklist overlap plus a matching or \
                near-matching title/date is strong evidence FOR same_identity, even with catalog \
                numbering/romanization/minor track-order differences (the same release often \
                gets catalogued independently by several providers). Evidence FOR \
                different_identity: a materially different tracklist/edition (e.g. a \
                Deluxe/Anniversary edition, a live/remix album, a separate regional release with \
                different content), release dates far apart with no edition link, or a generic \
                shared title (e.g. \"Various Artists\") that could label unrelated releases. \
                OVERRIDE: if `a` and `b` share a `release_group_ids` value, treat that as strong \
                evidence FOR different_identity instead -- MusicBrainz/Discogs deliberately split \
                one release_group into separate release entities to represent distinct \
                editions/pressings, so a shared group id means \"different edition\" far more \
                often than \"duplicate catalogued twice\", even when title/tracklist look alike.",
            &[
                (
                    "same_identity",
                    "same release (overlapping tracklist, matching title/date) catalogued \
                     independently by different providers -- merge.",
                ),
                (
                    "different_identity",
                    "different releases -- a materially different tracklist/edition, a shared \
                     release_group id (distinct editions), unrelated dates, or a generic shared \
                     title with no real tracklist overlap.",
                ),
            ],
        ),
        "release_group" => identity_choice(
            "`a`/`b` are release-group records -- the overarching creative work (e.g. \"the \
                album\") independent of any specific edition, pressing, or regional release. Are \
                they the SAME overarching work? Unlike at the `release` level, differences in \
                pressing, edition, bonus-track count, regional tracklist, or title \
                language/romanization do NOT matter here -- all still the same release_group. \
                Use different_identity only when the underlying creative work itself differs -- \
                a distinct album/EP project, an unrelated work sharing a generic title, or a \
                compilation vs. the original work it draws from.",
            &[
                (
                    "same_identity",
                    "same overarching creative work, regardless of edition/pressing differences \
                     between member releases -- merge.",
                ),
                (
                    "different_identity",
                    "different creative works -- not merely a different edition of the same one.",
                ),
            ],
        ),
        _ => identity_choice(
            "`a`/`b` are track records from multiple databases. Classify the relationship: \
                same_identity (the same recording -- merge), related_variant (a different \
                mix/arrangement/component of the same song -- e.g. Instrumental, Off Vocal, \
                Karaoke, Acapella, Remix, Arrange/Arrangement, a named lineup/event version -- \
                keep separate but linked), or unrelated (a different song, or an independent \
                performer's own separate recording sharing only the title -- no real \
                connection). Language, romanization, capitalization, an edition suffix like \
                \"(TV size)\", or an appended cover-credit tag (e.g. \"<title> ／ \
                <performer>(Cover)\", a common YouTube convention) are NOT evidence against \
                same_identity. A suffix naming a different mix/arrangement/component means NOT \
                same_identity, but IS related_variant, not unrelated -- don't collapse that \
                distinction. When base titles are effectively identical (ignoring punctuation \
                like full/half-width comma) with no version/arrangement suffix, default to \
                same_identity even without a duration match: a music-video cut commonly runs \
                10-30s longer than an audio cut of the same song, so that gap alone isn't \
                contradicting evidence -- and neither is partial credited-artist overlap, since \
                source data is often incomplete. EXCEPTION: a short generic label reused across \
                releases (numbered MC/talk segments, \"Intro\", \"Outro\", \"Encore\") is weak \
                evidence even on an exact match, since each event has its own distinct segment \
                under the same name -- require other corroboration, defaulting to unrelated or \
                unsure without it.",
            &[
                (
                    "same_identity",
                    "the same recording (the same actual audio content) -- merge.",
                ),
                (
                    "related_variant",
                    "a different version/mix/arrangement/component of the same song (e.g. \
                     instrumental vs. vocal, a named arrangement/lineup) -- related, not merged.",
                ),
                (
                    "unrelated",
                    "no real connection -- a different song, or an independent performer's own \
                     recording sharing only the title.",
                ),
            ],
        ),
    };
    q.insert("identity".to_string(), identity_question);
    // Only tracks have a `related_variant` identity answer, so only tracks
    // need this follow-up. Always asked alongside `identity` (answers come
    // back in one batched call) rather than conditioned on it -- give a
    // `not_applicable` escape hatch for when identity turns out to be
    // same_identity or unrelated instead.
    if entry_type == "track" {
        q.insert(
            "variant_kind".to_string(),
            identity_choice(
                "HARD RULE, check this first: if your `identity` answer for this pair was \
                    same_identity or unrelated, you MUST answer not_applicable here and skip the \
                    rest of this question -- do not pick a specific category just because `a` \
                    and `b` individually resemble that kind of thing in general (e.g. two \
                    independent artists who each separately cover the same song are unrelated as \
                    a PAIR even though each one is individually \"a cover\" -- with no evidenced \
                    connection between `a` and `b` specifically, the right answer is \
                    not_applicable, not a guess at a shared genre). Only when your `identity` \
                    answer was related_variant, proceed: what kind of version difference is it? \
                    Pick the single best-fitting category from the evidence (titles, aliases, \
                    durations), or unsure if they're clearly connected but the specific kind \
                    isn't evidenced.",
                &[
                    (
                        "cover",
                        "an independent performer's own vocal cover/rendition of the same song \
                         (different singer from the original).",
                    ),
                    (
                        "live",
                        "a live performance recording vs. a studio recording of the same song.",
                    ),
                    (
                        "remix",
                        "an official or fan remix / DJ edit of the same track.",
                    ),
                    (
                        "rearrangement",
                        "a musical rearrangement/reinterpretation (different instrumentation or \
                         style) of the same song, not a full remix.",
                    ),
                    (
                        "instrumental",
                        "an instrumental / off-vocal / karaoke version vs. the vocal version.",
                    ),
                    (
                        "edition",
                        "a different length/edition cut of the same recording (e.g. \"TV size\", \
                         a short version, an extended cut).",
                    ),
                    (
                        "lineup_version",
                        "a named alternate-lineup or event-specific version of the same song -- \
                         trigger on the STRUCTURAL pattern (one side has a parenthetical/named \
                         suffix like \"(X ver.)\"/\"(X Ver)\" naming some lineup/event/occasion \
                         that the other side lacks) even if you don't personally recognize what \
                         the specific name X refers to; you don't need world knowledge of the \
                         named event/lineup, only that it's a distinct named-occasion suffix.",
                    ),
                    (
                        "not_applicable",
                        "identity was same_identity or unrelated -- there is no version \
                         relationship between `a` and `b` to classify.",
                    ),
                ],
            ),
        );
        q.insert(
            "variant_direction".to_string(),
            identity_choice(
                "Same hard rule as `variant_kind`: if your `identity` answer for this pair was \
                    same_identity or unrelated, you MUST answer not_applicable here. Two more \
                    HARD RULES, check these before picking a side: (1) if `a` and `b`'s titles/ \
                    aliases are identical or differ only in trivial formatting/punctuation, with \
                    NO distinguishing version/transformation suffix on either side, you MUST \
                    answer not_applicable -- there is no textual basis to pick a direction, so do \
                    not guess one anyway. (2) if `a` and `b` are each independently credited to a \
                    DIFFERENT transformer/arranger/coverer (e.g. two different named arrangers, \
                    two different cover singers), with neither side's title claiming to be the \
                    untransformed base, answer not_applicable -- they are likely both derived \
                    from a common original that is neither `a` nor `b`, so don't assume one \
                    derives from the other just because they're linked. Only past both of those \
                    checks, proceed: of `a` and `b`, which one is the derived/transformed side \
                    (the cover, remix, arrangement, instrumental cut, edition cut, etc.), and \
                    which is closer to the original/base recording? Base this on evidence like \
                    which side's title names the transformation (\"(Instrumental)\", \"Arranged \
                    by X\", \"(Cover)\") versus which has the bare/plain title, or which side's \
                    title explicitly says \"original\".",
                &[
                    (
                        "a_is_derived",
                        "`a` is the derived/transformed version; `b` is closer to the \
                         original/base recording.",
                    ),
                    (
                        "b_is_derived",
                        "`b` is the derived/transformed version; `a` is closer to the \
                         original/base recording.",
                    ),
                    (
                        "not_applicable",
                        "identity was same_identity or unrelated, or a direction between `a` \
                         and `b` specifically isn't evidenced/doesn't apply.",
                    ),
                ],
            ),
        );
    }
    q.insert(
        "title_match".to_string(),
        Question::Noul {
            instructions: "Do a's and b's titles refer to the same underlying work, ignoring \
                language/romanization variants, formatting, and edition/version suffixes?"
                .to_string(),
        },
    );
    q.insert(
        "duration_consistent".to_string(),
        Question::Noul {
            instructions: "Are a's and b's reported durations consistent with being the same \
                recording, allowing for missing data on either side and normal \
                encoding/rounding variance (a few seconds)? If either side has no duration \
                data, treat that as not contradicting a match."
                .to_string(),
        },
    );
    q
}

/// Unlike `build_questions`'s `identity` Choice (same/different/unsure), this
/// targets the much larger silver-label set available at scale: the existing
/// heuristic pipeline's `kind="variant"` RELATE decisions (e.g. MV vs audio
/// cut, release editions) in `entry_relation`. Those pairs are deliberately
/// NOT the same identity (the pipeline chose to relate, not merge, them), so
/// a same/different/unsure Choice would score them wrong by construction --
/// this asks the narrower question the pipeline itself answered.
fn build_relatedness_questions() -> BTreeMap<String, Question> {
    let mut q = BTreeMap::new();
    q.insert(
        "related".to_string(),
        Question::Noul {
            instructions: "Entries `a` and `b` are catalog metadata records aggregated from \
                multiple independent music databases into one library, both of the same \
                `entry_type`. Are `a` and `b` connected as the same underlying musical work or \
                act in some form -- whether that means they are effectively the same \
                recording/release catalogued twice (e.g. the same song listed under two \
                different release entries), or clearly distinguishable versions, cuts, editions, \
                or credited appearances of it (e.g. a music-video cut vs. an audio-only cut of \
                the same song, different release editions, a cover linked to its original, or \
                the same artist under a different display name)? This is not asking whether they \
                should be merged into a single record -- only whether they represent the same \
                underlying work/act in some form, identical or not. Answer no if they are \
                unrelated works, or unrelated people/releases that merely happen to look \
                superficially similar."
                .to_string(),
        },
    );
    q.insert(
        "title_match".to_string(),
        Question::Noul {
            instructions: "Do a's and b's titles refer to the same underlying work, ignoring \
                language/romanization variants, formatting, and edition/version suffixes?"
                .to_string(),
        },
    );
    q.insert(
        "duration_consistent".to_string(),
        Question::Noul {
            instructions: "Are a's and b's reported durations consistent with being the same \
                recording, allowing for missing data on either side and normal \
                encoding/rounding variance (a few seconds)? If either side has no duration \
                data, treat that as not contradicting a match."
                .to_string(),
        },
    );
    q
}

/// Fetch `EntryInfo` for `ids` plus a second hop of everything they reference
/// (credited peers, release-track children, track-position releases) so
/// `entry_view` can resolve those ids to real titles instead of dropping them.
async fn load_entries_with_context(
    db: &MusicDb,
    ids: &[i64],
) -> anyhow::Result<HashMap<i64, EntryInfo>> {
    let mut entries = entry_infos_by_ids(db, ids).await?;
    let mut extra_ids: Vec<i64> = entries
        .values()
        .flat_map(|e| {
            e.peer_entry_ids
                .iter()
                .copied()
                .chain(e.child_entry_ids.iter().copied())
                .chain(e.track_positions.iter().map(|&(r, _, _)| r))
        })
        .filter(|id| !entries.contains_key(id))
        .collect();
    extra_ids.sort_unstable();
    extra_ids.dedup();
    if !extra_ids.is_empty() {
        let extra = entry_infos_by_ids(db, &extra_ids).await?;
        entries.extend(extra);
    }
    Ok(entries)
}

async fn call_typesafe(
    client: &reqwest::Client,
    api_key: &str,
    request: &TypesafeRequest,
) -> anyhow::Result<TypesafeResponse> {
    let resp = client
        .post(API_URL)
        .bearer_auth(api_key)
        .json(request)
        .send()
        .await
        .context("calling TypeSafe API")?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("HTTP {status}: {body}");
    }
    serde_json::from_str(&body).with_context(|| format!("parsing response: {body}"))
}

#[derive(Debug, Clone)]
struct PairRow {
    a: i64,
    b: i64,
    label: String,
    heuristic_confidence: Option<f64>,
}

fn parse_pairs_file(text: &str) -> anyhow::Result<Vec<PairRow>> {
    let mut rows = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || i == 0 && line.starts_with("entry_a") {
            continue;
        }
        let parts: Vec<&str> = line.split(',').collect();
        anyhow::ensure!(parts.len() >= 3, "bad line {}: {line:?}", i + 1);
        rows.push(PairRow {
            a: parts[0]
                .parse()
                .with_context(|| format!("line {}", i + 1))?,
            b: parts[1]
                .parse()
                .with_context(|| format!("line {}", i + 1))?,
            label: parts[2].to_string(),
            heuristic_confidence: parts.get(3).and_then(|s| s.parse().ok()),
        });
    }
    Ok(rows)
}

struct PairScore {
    row: PairRow,
    entry_type: String,
    related: f64,
    /// Raw `identity` Choice value when the question mode produces one (e.g.
    /// track's 3-way same_identity/related_variant/unrelated) -- lets
    /// downstream analysis distinguish "demote to RELATE" from "no
    /// connection at all" instead of collapsing both into `related = 0.0`.
    identity_choice: Option<String>,
    /// Raw `variant_kind` Choice value (track only) -- which kind of
    /// version difference a related_variant pair is (cover/live/remix/
    /// rearrangement/instrumental/edition/lineup_version/not_applicable).
    variant_kind: Option<String>,
    /// Raw `variant_direction` Choice value (track only) -- which of a/b is
    /// the derived/transformed side vs. the original/base
    /// (a_is_derived/b_is_derived/not_applicable).
    variant_direction: Option<String>,
    title_match: f64,
    duration_consistent: f64,
    input_tokens: u64,
    output_tokens: u64,
}

async fn run_relatedness_eval(
    db: &MusicDb,
    db_path: &std::path::Path,
    api_key: Option<&str>,
    dry_run: bool,
    path: &str,
    limit: usize,
    concurrency: usize,
    question_mode: &str,
) -> anyhow::Result<()> {
    let build_questions_for_mode: Box<dyn Fn(&str) -> BTreeMap<String, Question> + Send + Sync> =
        match question_mode {
            "related" => Box::new(|_entry_type: &str| build_relatedness_questions()),
            "identity" => Box::new(build_questions),
            "verdict" => Box::new(|_entry_type: &str| build_verdict_questions()),
            other => anyhow::bail!(
                "unknown --questions {other:?}, expected \"related\", \"identity\", or \"verdict\""
            ),
        };
    let build_questions_for_mode = Arc::new(build_questions_for_mode);
    if question_mode == "verdict" && !dry_run {
        anyhow::bail!(
            "--questions verdict is a design sketch (see response_to_verdict/build_verdict_questions) \
             -- only wired for --dry-run, not live scoring/aggregation"
        );
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    let mut rows = parse_pairs_file(&text)?;
    if limit > 0 {
        rows.truncate(limit);
    }
    info_line(&format!("{} pair(s) to evaluate from {path}", rows.len()));

    let ids: Vec<i64> = rows.iter().flat_map(|r| [r.a, r.b]).collect();
    let entries = Arc::new(load_entries_with_context(db, &ids).await?);
    let release_groups = Arc::new(load_release_groups(db_path, &ids)?);

    if dry_run {
        for row in rows.iter().take(3) {
            let (Some(ea), Some(eb)) = (entries.get(&row.a), entries.get(&row.b)) else {
                continue;
            };
            let request = TypesafeRequest {
                state: StatePayload {
                    a: entry_view(ea, &entries, &release_groups),
                    b: entry_view(eb, &entries, &release_groups),
                },
                model: MODEL.to_string(),
                questions: build_questions_for_mode(&ea.entry_type),
            };
            println!("--- pair ({}, {})  label={} ---", row.a, row.b, row.label);
            println!("{}", serde_json::to_string_pretty(&request)?);
        }
        eprintln!(
            "(dry-run: showed 3 of {} pairs; re-run with a key to score all of them)",
            rows.len()
        );
        return Ok(());
    }

    let client = reqwest::Client::new();
    let api_key = api_key.unwrap().to_string();

    let results: Vec<anyhow::Result<PairScore>> = stream::iter(rows.into_iter())
        .map(|row| {
            let client = client.clone();
            let api_key = api_key.clone();
            let entries = Arc::clone(&entries);
            let release_groups = Arc::clone(&release_groups);
            let build_questions_for_mode = Arc::clone(&build_questions_for_mode);
            async move {
                let (Some(ea), Some(eb)) = (entries.get(&row.a), entries.get(&row.b)) else {
                    anyhow::bail!("entry missing for pair ({}, {})", row.a, row.b);
                };
                let entry_type = ea.entry_type.clone();
                let request = TypesafeRequest {
                    state: StatePayload {
                        a: entry_view(ea, &entries, &release_groups),
                        b: entry_view(eb, &entries, &release_groups),
                    },
                    model: MODEL.to_string(),
                    questions: build_questions_for_mode(&entry_type),
                };
                let parsed = call_typesafe(&client, &api_key, &request)
                    .await
                    .with_context(|| format!("pair ({}, {})", row.a, row.b))?;
                let get_noul = |id: &str| match parsed.answers.get(id) {
                    Some(Answer::Noul { noul }) => *noul,
                    _ => f64::NAN,
                };
                // "identity" mode's main answer is a Choice, not a Noul -- fold it into the
                // same 0..1 "related" slot (1.0 = same_identity) so the existing aggregation
                // and CSV output work unchanged for ad hoc identity-mode checks. The raw choice
                // string is kept separately (identity_choice) so a 3-way vocabulary like track's
                // same_identity/related_variant/unrelated isn't lossily collapsed to related=0.0
                // for both "demote to RELATE" and "no connection at all".
                let mut identity_choice = None;
                let related = match parsed.answers.get("identity") {
                    Some(Answer::Choice { choice, .. }) => {
                        identity_choice = Some(choice.clone());
                        if choice == "same_identity" { 1.0 } else { 0.0 }
                    }
                    _ => get_noul("related"),
                };
                let variant_kind = match parsed.answers.get("variant_kind") {
                    Some(Answer::Choice { choice, .. }) => Some(choice.clone()),
                    _ => None,
                };
                let variant_direction = match parsed.answers.get("variant_direction") {
                    Some(Answer::Choice { choice, .. }) => Some(choice.clone()),
                    _ => None,
                };
                Ok(PairScore {
                    related,
                    identity_choice,
                    variant_kind,
                    variant_direction,
                    title_match: get_noul("title_match"),
                    duration_consistent: get_noul("duration_consistent"),
                    input_tokens: parsed.usage.input_tokens,
                    output_tokens: parsed.usage.output_tokens,
                    row,
                    entry_type,
                })
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    let mut scores = Vec::new();
    let mut total_in = 0u64;
    let mut total_out = 0u64;
    for r in results {
        match r {
            Ok(s) => {
                total_in += s.input_tokens;
                total_out += s.output_tokens;
                scores.push(s);
            }
            Err(e) => eprintln!("skip: {e:#}"),
        }
    }

    report_relatedness(&scores);
    let scored_path = format!("{path}.scored.csv");
    write_scored_csv(&scored_path, &scores)?;
    println!("\nPer-pair scores written to {scored_path}");
    println!(
        "\nUsage: {total_in} input tokens, {total_out} output tokens across {} call(s).",
        scores.len()
    );
    println!(
        "Cost at $0.042/1M input tokens, output free: ${:.6}",
        total_in as f64 * INPUT_USD_PER_TOKEN
    );
    Ok(())
}

fn write_scored_csv(path: &str, scores: &[PairScore]) -> anyhow::Result<()> {
    use std::io::Write as _;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    writeln!(
        f,
        "entry_a,entry_b,label,heuristic_confidence,entry_type,related,identity_choice,variant_kind,variant_direction,title_match,duration_consistent"
    )?;
    for s in scores {
        writeln!(
            f,
            "{},{},{},{},{},{:.3},{},{},{},{:.3},{:.3}",
            s.row.a,
            s.row.b,
            s.row.label,
            s.row
                .heuristic_confidence
                .map(|c| c.to_string())
                .unwrap_or_default(),
            s.entry_type,
            s.related,
            s.identity_choice.as_deref().unwrap_or(""),
            s.variant_kind.as_deref().unwrap_or(""),
            s.variant_direction.as_deref().unwrap_or(""),
            s.title_match,
            s.duration_consistent,
        )?;
    }
    Ok(())
}

fn report_relatedness(scores: &[PairScore]) {
    let mut by_group: BTreeMap<(String, String), Vec<&PairScore>> = BTreeMap::new();
    for s in scores {
        by_group
            .entry((s.row.label.clone(), s.entry_type.clone()))
            .or_default()
            .push(s);
    }
    println!(
        "\n{:<10} {:<10} {:>5} {:>10} {:>8} {:>8}",
        "label", "type", "n", "mean_rel", "%>=0.5", "%>=0.8"
    );
    for ((label, entry_type), group) in &by_group {
        let n = group.len() as f64;
        let mean: f64 = group.iter().map(|s| s.related).sum::<f64>() / n;
        let pct_50 = group.iter().filter(|s| s.related >= 0.5).count() as f64 / n * 100.0;
        let pct_80 = group.iter().filter(|s| s.related >= 0.8).count() as f64 / n * 100.0;
        println!(
            "{label:<10} {entry_type:<10} {:>5} {mean:>10.3} {pct_50:>7.1}% {pct_80:>7.1}%",
            group.len()
        );
    }

    // For the positive (heuristic RELATE) group, bucket by the existing
    // pipeline's own confidence to see whether Jev's agreement tracks it.
    let positives: Vec<&PairScore> = scores
        .iter()
        .filter(|s| s.row.heuristic_confidence.is_some())
        .collect();
    if !positives.is_empty() {
        println!("\n{:<14} {:>5} {:>10}", "heuristic_conf", "n", "mean_rel");
        for (label, lo, hi) in [
            ("< 0.75", 0.0, 0.75),
            ("0.75-0.9", 0.75, 0.9),
            (">= 0.9", 0.9, 1.01),
        ] {
            let bucket: Vec<&&PairScore> = positives
                .iter()
                .filter(|s| {
                    let c = s.row.heuristic_confidence.unwrap();
                    c >= lo && c < hi
                })
                .collect();
            if bucket.is_empty() {
                continue;
            }
            let n = bucket.len() as f64;
            let mean: f64 = bucket.iter().map(|s| s.related).sum::<f64>() / n;
            println!("{label:<14} {:>5} {mean:>10.3}", bucket.len());
        }
    }
}

// ── Design sketch: Jev deciding `Verdict` directly ──────────────────────────
//
// Not wired into any CLI path or the real pipeline -- this demonstrates that
// a Jev Choice answer can drop straight into `pipeline::softmatch::Verdict`,
// the exact type `apply_candidate` consumes from `call_script`/
// `DedupModel::decide` today. See the unit test below for an end-to-end
// example. Two things this sketch does NOT solve, both real requirements for
// an actual integration (see [[typesafe-soft-dedup-poc-findings]] memory):
//  1. Jev returns no free-text explanation (System One models give typed
//     answers/probabilities, not reasoning traces), so `reason` here is
//     synthesized from the diagnostic Nouls' own values, not a real
//     explanation the way the Rhai script's `reason` strings are.
//  2. No idempotency cache: calling this per pass would violate
//     [[softmatch-idempotency-invariant]] near confidence boundaries where
//     Jev isn't fully deterministic (observed directly in this PoC: pair
//     (222,844) flipped `identity` choice on an identical repeat call before
//     the evidence was enriched). A real integration needs verdicts cached
//     per `(pair, evidence-hash)`, not re-queried every `softmatch --apply`.

/// Mirrors `Verdict`'s decision space directly: MERGE / RELATE("variant") /
/// DISTINCT / DEFER. "variant" is the only `kind` the Rhai script ever emits
/// today (`rg 'relate\(' config/match.example.rhai`), so there's no richer
/// kind taxonomy to model yet.
fn build_verdict_questions() -> BTreeMap<String, Question> {
    let mut q = BTreeMap::new();
    q.insert(
        "verdict".to_string(),
        Question::Choice {
            instructions: "Entries `a` and `b` are catalog metadata records aggregated from \
                multiple independent music databases into one library, both of the same \
                `entry_type`. Decide how they should be handled in the library: merged into one \
                record, kept separate but linked as related (different cut/edition/credited- \
                appearance of the same underlying work, or the same act under a different \
                display name), left as unrelated distinct records, or deferred if the evidence \
                is genuinely insufficient to decide either way."
                .to_string(),
            criteria: BTreeMap::from([
                (
                    "merge".to_string(),
                    "a and b are the same underlying entity and should be collapsed into a \
                     single record."
                        .to_string(),
                ),
                (
                    "relate_variant".to_string(),
                    "a and b are meaningfully connected (different cut/edition/credited- \
                     appearance of the same work, or the same act under a different name) but \
                     should remain separate records."
                        .to_string(),
                ),
                (
                    "distinct".to_string(),
                    "a and b are unrelated works or acts.".to_string(),
                ),
                (
                    "defer".to_string(),
                    "the evidence is genuinely insufficient to decide.".to_string(),
                ),
            ]),
        },
    );
    q.insert(
        "title_match".to_string(),
        Question::Noul {
            instructions: "Do a's and b's titles refer to the same underlying work, ignoring \
                language/romanization variants, formatting, and edition/version suffixes?"
                .to_string(),
        },
    );
    q.insert(
        "duration_consistent".to_string(),
        Question::Noul {
            instructions: "Are a's and b's reported durations consistent with being the same \
                recording, allowing for missing data on either side and normal \
                encoding/rounding variance (a few seconds)? If either side has no duration \
                data, treat that as not contradicting a match."
                .to_string(),
        },
    );
    q
}

/// What a real `apply_candidate` integration would call instead of
/// `call_script`/`DedupModel::decide`. Returns `None` if the response is
/// missing the `verdict` answer (shouldn't happen for a well-formed request).
/// Exercised by the tests below; not called from `main` since this sketch
/// isn't wired into live scoring -- see `run_relatedness_eval`'s guard.
#[allow(dead_code)]
fn response_to_verdict(parsed: &TypesafeResponse) -> Option<Verdict> {
    let Some(Answer::Choice {
        choice, confidence, ..
    }) = parsed.answers.get("verdict")
    else {
        return None;
    };
    let noul = |id: &str| match parsed.answers.get(id) {
        Some(Answer::Noul { noul }) => *noul,
        _ => f64::NAN,
    };
    let reason = format!(
        "jev-latest (title_match={:.2}, duration_consistent={:.2})",
        noul("title_match"),
        noul("duration_consistent"),
    );
    Some(match choice.as_str() {
        "merge" => Verdict::Merge {
            confidence: *confidence,
            reason,
        },
        "relate_variant" => Verdict::Relate {
            kind: "variant".to_string(),
            confidence: *confidence,
            reason,
            metadata: None,
        },
        "distinct" => Verdict::Distinct,
        _ => Verdict::Defer {
            confidence: *confidence,
            reason,
        },
    })
}

#[cfg(test)]
mod verdict_sketch_tests {
    use super::*;

    fn response(choice: &str, confidence: f64) -> TypesafeResponse {
        let mut answers = BTreeMap::new();
        answers.insert(
            "verdict".to_string(),
            Answer::Choice {
                choice: choice.to_string(),
                probabilities: BTreeMap::new(),
                confidence,
            },
        );
        answers.insert("title_match".to_string(), Answer::Noul { noul: 0.9 });
        answers.insert(
            "duration_consistent".to_string(),
            Answer::Noul { noul: 0.85 },
        );
        TypesafeResponse {
            answers,
            usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
            },
        }
    }

    #[test]
    fn merge_choice_becomes_merge_verdict() {
        let v = response_to_verdict(&response("merge", 0.92)).unwrap();
        assert!(matches!(v, Verdict::Merge { confidence, .. } if confidence == 0.92));
    }

    #[test]
    fn relate_variant_choice_becomes_relate_verdict() {
        let v = response_to_verdict(&response("relate_variant", 0.8)).unwrap();
        assert!(matches!(v, Verdict::Relate { kind, .. } if kind == "variant"));
    }

    #[test]
    fn distinct_choice_becomes_distinct_verdict() {
        let v = response_to_verdict(&response("distinct", 0.99)).unwrap();
        assert!(matches!(v, Verdict::Distinct));
    }

    #[test]
    fn unknown_choice_defers() {
        let v = response_to_verdict(&response("unsure", 0.4)).unwrap();
        assert!(matches!(v, Verdict::Defer { .. }));
    }
}

// ── Merge ground-truth eval ──────────────────────────────────────────────
//
// The `entry_relation` silver labels (used by `run_relatedness_eval`) only
// cover the heuristic's RELATE ("variant") decisions -- there was never a
// labeled sample for the pipeline's other big decision, MERGE. This mode
// builds one for free: any entry already carrying pairs from >=2 distinct
// sources (e.g. a spotify pair and a musicbrainz pair) is proof the
// pipeline already decided those source records are the same identity --
// that's not an approximation, it's the DB's own current state. Splitting
// such an entry's pairs into two per-source "halves" and asking Jev
// whether they're the same identity gives exact MERGE ground truth (always
// "yes") at zero extra recovery cost. Negatives are built the same
// restricted (single-source) way from random distinct entries, for a fair
// comparison against evidence of the same shape.
//
// Deliberately reuses `build_questions()`'s `identity` Choice (not the
// `related` Noul from `run_relatedness_eval`) since "should this MERGE" is
// exactly what that Choice was designed to answer -- this is the first
// eval in this PoC with real ground truth for that specific question.

struct MergeEvalCase {
    label: &'static str, // "positive" | "negative"
    entry_type: String,
    desc: String,
    a: EntryView,
    b: EntryView,
}

fn sample_multi_source_entries(
    conn: &SqliteConnection,
    entry_type: &str,
    n: usize,
) -> anyhow::Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT e.id FROM entry e \
         JOIN (SELECT entry_id, COUNT(DISTINCT source) AS nsrc FROM entry_source GROUP BY entry_id) s \
           ON s.entry_id = e.id \
         WHERE e.entry_type = ?1 AND s.nsrc >= 2 \
         ORDER BY RANDOM() LIMIT ?2",
    )?;
    let ids = stmt
        .query_map(rusqlite::params![entry_type, n as i64], |row| row.get(0))?
        .collect::<Result<Vec<i64>, _>>()?;
    Ok(ids)
}

/// The two sources with the most `entry_source` rows for this entry --
/// picking the best-populated pair gives the richest, most realistic split.
fn top_two_sources(
    conn: &SqliteConnection,
    entry_id: i64,
) -> anyhow::Result<Option<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT source, COUNT(*) as n FROM entry_source WHERE entry_id = ?1 \
         GROUP BY source ORDER BY n DESC LIMIT 2",
    )?;
    let sources = stmt
        .query_map(rusqlite::params![entry_id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(match sources.as_slice() {
        [a, b, ..] => Some((a.clone(), b.clone())),
        _ => None,
    })
}

/// The single best-populated source for this entry -- prefers a source
/// likely to actually carry alias/title data (isrc/unknown_url etc. often
/// don't) over a purely random pick, same heuristic as `top_two_sources`.
fn any_source(conn: &SqliteConnection, entry_id: i64) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT source FROM entry_source WHERE entry_id = ?1 \
             GROUP BY source ORDER BY COUNT(*) DESC LIMIT 1",
            rusqlite::params![entry_id],
            |row| row.get(0),
        )
        .ok())
}

/// One entry's view restricted to a single source -- built straight from
/// `entry_source`/`entry_alias`, bypassing `EntryInfo`'s cross-source
/// aggregation entirely (that's the whole point: reconstruct what one
/// source's standalone record looked like before it got merged in).
/// `credited_names`/`track_positions`/`release_tracks` are left empty --
/// those come from contribution/child edges tied to the *entry*, not a
/// single source, so including them would leak the answer.
fn restricted_entry_view(
    conn: &SqliteConnection,
    entry_id: i64,
    source: &str,
    entry_type: &str,
) -> anyhow::Result<EntryView> {
    let mut alias_stmt = conn.prepare(
        "SELECT DISTINCT ea.name FROM entry_alias ea \
         JOIN entry_source es ON es.source = ea.source AND es.identifier = ea.identifier \
         WHERE es.entry_id = ?1 AND es.source = ?2 LIMIT ?3",
    )?;
    let titles: Vec<TitledAlias> = alias_stmt
        .query_map(
            rusqlite::params![entry_id, source, TOP_TITLES as i64],
            |row| {
                Ok(TitledAlias {
                    source: source.to_string(),
                    name: row.get(0)?,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;

    let mut src_stmt = conn.prepare(
        "SELECT duration_ms, release_date, release_type, primary_type \
         FROM entry_source WHERE entry_id = ?1 AND source = ?2",
    )?;
    let rows: Vec<(Option<i64>, Option<String>, Option<String>, Option<String>)> = src_stmt
        .query_map(rusqlite::params![entry_id, source], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut durations_sec: Vec<f64> = rows
        .iter()
        .filter_map(|r| r.0)
        .map(|ms| ms as f64 / 1000.0)
        .collect();
    durations_sec.sort_by(|a, b| a.total_cmp(b));
    durations_sec.dedup();
    let mut release_dates: Vec<String> = rows.iter().filter_map(|r| r.1.clone()).collect();
    release_dates.sort();
    release_dates.dedup();
    let mut release_types: Vec<String> = rows.iter().filter_map(|r| r.2.clone()).collect();
    release_types.sort();
    release_types.dedup();
    let mut primary_types: Vec<String> = rows.iter().filter_map(|r| r.3.clone()).collect();
    primary_types.sort();
    primary_types.dedup();

    Ok(EntryView {
        entry_type: entry_type.to_string(),
        titles,
        durations_sec,
        release_dates,
        release_types,
        primary_types,
        sources: vec![source.to_string()],
        credited_names: Vec::new(),
        track_positions: Vec::new(),
        release_tracks: Vec::new(),
        handles: Vec::new(),
        release_group_ids: Vec::new(),
    })
}

fn build_merge_eval_cases(
    conn: &SqliteConnection,
    entry_type: &str,
    n: usize,
) -> anyhow::Result<Vec<MergeEvalCase>> {
    let mut cases = Vec::new();

    // Positives: split one entry's own pairs by source.
    for entry_id in sample_multi_source_entries(conn, entry_type, n)? {
        let Some((src_a, src_b)) = top_two_sources(conn, entry_id)? else {
            continue;
        };
        let a = restricted_entry_view(conn, entry_id, &src_a, entry_type)?;
        let b = restricted_entry_view(conn, entry_id, &src_b, entry_type)?;
        if a.titles.is_empty() || b.titles.is_empty() {
            continue; // no alias text to judge on -- not a useful case
        }
        cases.push(MergeEvalCase {
            label: "positive",
            entry_type: entry_type.to_string(),
            desc: format!("entry {entry_id}: {src_a} vs {src_b}"),
            a,
            b,
        });
    }

    // Negatives: two random distinct entries, no entry_relation between
    // them, each restricted to one of its own sources the same way.
    let mut stmt = conn.prepare(
        "WITH t AS (SELECT id FROM entry WHERE entry_type = ?1) \
         SELECT a.id, b.id FROM t a JOIN t b ON a.id < b.id \
         WHERE NOT EXISTS ( \
           SELECT 1 FROM entry_relation r \
           WHERE (r.entry_a = a.id AND r.entry_b = b.id) \
              OR (r.entry_a = b.id AND r.entry_b = a.id) \
         ) ORDER BY RANDOM() LIMIT ?2",
    )?;
    let neg_pairs: Vec<(i64, i64)> = stmt
        .query_map(rusqlite::params![entry_type, n as i64], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (id_a, id_b) in neg_pairs {
        let (Some(src_a), Some(src_b)) = (any_source(conn, id_a)?, any_source(conn, id_b)?) else {
            continue;
        };
        let a = restricted_entry_view(conn, id_a, &src_a, entry_type)?;
        let b = restricted_entry_view(conn, id_b, &src_b, entry_type)?;
        if a.titles.is_empty() || b.titles.is_empty() {
            continue;
        }
        cases.push(MergeEvalCase {
            label: "negative",
            entry_type: entry_type.to_string(),
            desc: format!("entry {id_a}:{src_a} vs entry {id_b}:{src_b}"),
            a,
            b,
        });
    }

    Ok(cases)
}

struct MergeEvalResult {
    case: MergeEvalCase,
    choice: String,
    confidence: f64,
    input_tokens: u64,
    output_tokens: u64,
}

async fn run_merge_eval(
    db_path: &std::path::Path,
    api_key: Option<&str>,
    dry_run: bool,
    concurrency: usize,
    n_per_type: usize,
) -> anyhow::Result<()> {
    let conn =
        SqliteConnection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening {} read-only via rusqlite", db_path.display()))?;

    let mut cases = Vec::new();
    for entry_type in ["track", "release", "artist"] {
        cases.extend(build_merge_eval_cases(&conn, entry_type, n_per_type)?);
    }
    drop(conn); // done with SQLite; the rest is pure network I/O
    info_line(&format!("{} merge-eval case(s) built", cases.len()));

    if dry_run {
        for case in cases.iter().take(3) {
            let request = TypesafeRequest {
                state: StatePayload {
                    a: case.a.clone(),
                    b: case.b.clone(),
                },
                model: MODEL.to_string(),
                questions: build_questions(&case.entry_type),
            };
            println!("--- {} case: {} ---", case.label, case.desc);
            println!("{}", serde_json::to_string_pretty(&request)?);
        }
        eprintln!(
            "(dry-run: showed 3 of {} cases; re-run with a key to score all of them)",
            cases.len()
        );
        return Ok(());
    }

    let client = reqwest::Client::new();
    let api_key = api_key.unwrap().to_string();

    let results: Vec<anyhow::Result<MergeEvalResult>> = stream::iter(cases.into_iter())
        .map(|case| {
            let client = client.clone();
            let api_key = api_key.clone();
            async move {
                let request = TypesafeRequest {
                    state: StatePayload {
                        a: case.a.clone(),
                        b: case.b.clone(),
                    },
                    model: MODEL.to_string(),
                    questions: build_questions(&case.entry_type),
                };
                let parsed = call_typesafe(&client, &api_key, &request)
                    .await
                    .with_context(|| format!("case: {}", case.desc))?;
                let (choice, confidence) = match parsed.answers.get("identity") {
                    Some(Answer::Choice {
                        choice, confidence, ..
                    }) => (choice.clone(), *confidence),
                    _ => ("?".to_string(), f64::NAN),
                };
                Ok(MergeEvalResult {
                    choice,
                    confidence,
                    input_tokens: parsed.usage.input_tokens,
                    output_tokens: parsed.usage.output_tokens,
                    case,
                })
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    let mut rows = Vec::new();
    let mut total_in = 0u64;
    let mut total_out = 0u64;
    for r in results {
        match r {
            Ok(row) => {
                total_in += row.input_tokens;
                total_out += row.output_tokens;
                rows.push(row);
            }
            Err(e) => eprintln!("skip: {e:#}"),
        }
    }

    let mut by_group: BTreeMap<(&str, String), Vec<&MergeEvalResult>> = BTreeMap::new();
    for r in &rows {
        by_group
            .entry((r.case.label, r.case.entry_type.clone()))
            .or_default()
            .push(r);
    }
    println!(
        "\n{:<10} {:<10} {:>5} {:>12} {:>10}",
        "label", "type", "n", "%same_id", "mean_conf"
    );
    for ((label, entry_type), group) in &by_group {
        let n = group.len() as f64;
        let pct_same =
            group.iter().filter(|r| r.choice == "same_identity").count() as f64 / n * 100.0;
        let mean_conf: f64 = group.iter().map(|r| r.confidence).sum::<f64>() / n;
        println!(
            "{label:<10} {entry_type:<10} {:>5} {pct_same:>11.1}% {mean_conf:>10.3}",
            group.len()
        );
    }

    // Positives should score same_identity (ground truth by construction);
    // negatives should not. Surface the disagreements directly since n is
    // small enough to read by hand.
    for r in &rows {
        let expected_same = r.case.label == "positive";
        let got_same = r.choice == "same_identity";
        if expected_same != got_same {
            println!(
                "DISAGREE [{}] {}: jev said {} (conf {:.2})",
                r.case.label, r.case.desc, r.choice, r.confidence
            );
        }
    }

    println!(
        "\nUsage: {total_in} input tokens, {total_out} output tokens across {} call(s).",
        rows.len()
    );
    println!(
        "Cost at $0.042/1M input tokens, output free: ${:.6}",
        total_in as f64 * INPUT_USD_PER_TOKEN
    );
    Ok(())
}

fn latest_judgment_per_pair(feedback: &[DedupFeedbackRow]) -> Vec<(i64, i64, String)> {
    let mut latest: HashMap<(i64, i64), &DedupFeedbackRow> = HashMap::new();
    for row in feedback {
        let key = (row.entry_a.min(row.entry_b), row.entry_a.max(row.entry_b));
        latest
            .entry(key)
            .and_modify(|cur| {
                if row.created_at > cur.created_at {
                    *cur = row;
                }
            })
            .or_insert(row);
    }
    let mut pairs: Vec<(i64, i64, String)> = latest
        .into_iter()
        .map(|((a, b), r)| (a, b, r.judgment.clone()))
        .collect();
    pairs.sort();
    pairs
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();

    let args = Args::parse();
    let api_key = std::env::var("TYPESAFE_API_KEY").ok();
    let dry_run = args.dry_run || api_key.is_none();
    if dry_run {
        eprintln!(
            "TYPESAFE_API_KEY not set -- dry-run mode: printing request bodies, no network calls.\n"
        );
    }

    let db_path = args
        .db
        .map(PathBuf::from)
        .unwrap_or_else(|| app_dirs::data_dir().join("musiclib.db"));

    if args.merge_eval {
        return run_merge_eval(
            &db_path,
            api_key.as_deref(),
            dry_run,
            args.concurrency,
            args.merge_eval_n,
        )
        .await;
    }

    let db = MusicDb::new(&format!("sqlite://{}?mode=ro", db_path.display()))
        .await
        .with_context(|| format!("opening {} read-only", db_path.display()))?;

    if let Some(path) = &args.pairs_file {
        return run_relatedness_eval(
            &db,
            &db_path,
            api_key.as_deref(),
            dry_run,
            path,
            args.limit,
            args.concurrency,
            &args.questions,
        )
        .await;
    }

    let feedback = db.all_dedup_feedback().await?;
    let mut pairs = latest_judgment_per_pair(&feedback);
    if let Some(spec) = &args.pairs {
        let wanted: Vec<(i64, i64)> = spec
            .split(',')
            .map(|s| {
                let (a, b) = s
                    .trim()
                    .split_once('-')
                    .with_context(|| format!("bad --pairs entry {s:?}, expected A-B"))?;
                anyhow::Ok((a.parse::<i64>()?, b.parse::<i64>()?))
            })
            .collect::<anyhow::Result<Vec<_>>>()?
            .into_iter()
            .map(|(a, b)| (a.min(b), a.max(b)))
            .collect();
        pairs.retain(|(a, b, _)| wanted.contains(&(*a, *b)));
    } else if args.limit > 0 {
        pairs.truncate(args.limit);
    }
    if pairs.is_empty() {
        anyhow::bail!("no dedup_feedback rows found in {}", db_path.display());
    }
    info_line(&format!(
        "{} human-labeled pair(s) (latest judgment wins per pair)",
        pairs.len()
    ));

    let ids: Vec<i64> = pairs.iter().flat_map(|(a, b, _)| [*a, *b]).collect();
    let entries = load_entries_with_context(&db, &ids).await?;
    let release_groups = load_release_groups(&db_path, &ids)?;

    let client = reqwest::Client::new();
    let mut correct = 0usize;
    let mut scored = 0usize;
    let mut total_in = 0u64;
    let mut total_out = 0u64;

    println!(
        "{:<6} {:<6} {:<8} {:<18} {:<18} {:<7} {}",
        "A", "B", "type", "human", "jev", "agree", "latency"
    );

    for (a_id, b_id, human) in &pairs {
        let (Some(ea), Some(eb)) = (entries.get(a_id), entries.get(b_id)) else {
            eprintln!(
                "skip ({a_id}, {b_id}): entry missing (merged/deleted since feedback was recorded)"
            );
            continue;
        };
        let request = TypesafeRequest {
            state: StatePayload {
                a: entry_view(ea, &entries, &release_groups),
                b: entry_view(eb, &entries, &release_groups),
            },
            model: MODEL.to_string(),
            questions: build_questions(&ea.entry_type),
        };

        if dry_run || args.verbose {
            println!("--- pair ({a_id}, {b_id})  human={human} ---");
            println!("{}", serde_json::to_string_pretty(&request)?);
        }
        if dry_run {
            continue;
        }

        let started = Instant::now();
        let resp = client
            .post(API_URL)
            .bearer_auth(api_key.as_deref().unwrap())
            .json(&request)
            .send()
            .await
            .with_context(|| format!("calling TypeSafe API for pair ({a_id}, {b_id})"))?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            eprintln!("pair ({a_id}, {b_id}): HTTP {status}: {body}");
            continue;
        }
        let parsed: TypesafeResponse = serde_json::from_str(&body)
            .with_context(|| format!("parsing response for ({a_id}, {b_id}): {body}"))?;

        total_in += parsed.usage.input_tokens;
        total_out += parsed.usage.output_tokens;

        let jev_choice = match parsed.answers.get("identity") {
            Some(Answer::Choice { choice, .. }) => choice.clone(),
            _ => "?".to_string(),
        };
        if args.verbose {
            for (id, answer) in &parsed.answers {
                println!("  {id}: {answer:?}");
            }
        }
        scored += 1;
        let agree = &jev_choice == human;
        if agree {
            correct += 1;
        }

        println!(
            "{a_id:<6} {b_id:<6} {:<8} {human:<18} {jev_choice:<18} {:<7} {:.2?}",
            ea.entry_type,
            if agree { "yes" } else { "no" },
            started.elapsed(),
        );
    }

    if !dry_run && scored > 0 {
        println!(
            "\n{correct}/{scored} pairs where Jev's `identity` choice matched the human label \
             (3-way: same_identity/different_identity/unsure)."
        );
        println!(
            "Usage: {total_in} input tokens, {total_out} output tokens across {scored} call(s)."
        );
        println!(
            "Cost at $0.042/1M input tokens, output free: ${:.6}",
            total_in as f64 * INPUT_USD_PER_TOKEN
        );
    }

    Ok(())
}

fn info_line(msg: &str) {
    eprintln!("[typesafe_poc] {msg}");
}
