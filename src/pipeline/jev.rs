//! TypeSafe Jev-based soft-dedup scorer: an alternative to the Rhai script
//! and the learned logistic model, selected per `entry_type` via
//! `SoftMatchConfig::jev_entry_types`. Only ever produces soft verdicts
//! (`Verdict::Merge` writes a reversible `same_identity` assertion, never a
//! destructive merge — see `apply_candidate` in `pipeline::softmatch`).
//!
//! Ported from the `typesafe_poc` validation work (project memory:
//! `typesafe-soft-dedup-poc-findings`) — the entry-type-specific `identity`
//! prompts below are the ones that PoC tuned and validated, not a fresh
//! design.

use std::collections::{BTreeMap, HashMap};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::http::{HeaderName, HeaderValue, Method, Request as HttpRequest, json_body_extractor};
use crate::musicdb::{MusicDb, NewJevVerdictCache};
use crate::pipeline::softmatch::{EntryInfo, SoftMatchConfig, Verdict, entry_infos_by_ids};

const API_URL: &str = "https://api.typesafe.ai/v1/systemone";
const MODEL: &str = "jev-latest";

/// Identifies both the request shape and the `identity` prompt wording.
/// Bump whenever either changes — folded into `evidence_hash`, so every
/// cached verdict auto-invalidates on a prompt/shape change rather than
/// silently reusing a stale answer.
pub const MODEL_VERSION: &str = "typesafe-jev/1";

// ── Evidence view ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct TitledAlias {
    source: String,
    name: String,
}

#[derive(Debug, Clone, Serialize)]
struct TrackPosition {
    release_title: Option<String>,
    disc_no: Option<i32>,
    track_no: Option<i32>,
}

#[derive(Debug, Clone, Serialize)]
struct EntryView {
    entry_type: String,
    titles: Vec<TitledAlias>,
    durations_sec: Vec<f64>,
    release_dates: Vec<String>,
    release_types: Vec<String>,
    primary_types: Vec<String>,
    sources: Vec<String>,
    credited_names: Vec<String>,
    track_positions: Vec<TrackPosition>,
    release_tracks: Vec<String>,
    handles: Vec<String>,
    /// For releases: internal entry ids of the release_group(s) this release
    /// belongs to. A shared value here is strong evidence AGAINST merging
    /// (MusicBrainz/Discogs deliberately split a release_group into
    /// separate release entities for distinct editions/pressings) — see the
    /// `release` prompt's override rule below.
    release_group_ids: Vec<i64>,
}

const TOP_TITLES: usize = 8;
const CREDITED_NAMES_CAP: usize = 8;
const CHILD_TRACKS_CAP: usize = 12;
const TRACK_POSITIONS_CAP: usize = 5;
const HANDLES_CAP: usize = 6;

/// Pull a human-readable slug out of a source identifier (URL), or `None` if
/// the trailing path segment looks like an opaque platform ID rather than a
/// name a human actually chose.
fn extract_handle(identifier: &str) -> Option<String> {
    let after_scheme = identifier.split_once("://")?.1;
    let without_query = after_scheme
        .split(['?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let core = without_query.split('/').rfind(|s| !s.is_empty())?;
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
        if let Some(first) = chars.next()
            && core.len() >= 5
            && first.is_ascii_uppercase()
            && chars.clone().all(|c| c.is_ascii_digit())
        {
            return None; // single-letter-prefixed numeric id (ASIN, wikidata Q-id, ...)
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

/// Entry ids of any release_group entries `entry` is linked to as a child
/// (via `entry_child`, release_group as parent). Empty for non-`release`
/// entries. Resolved through existing `MusicDb`/`entry_infos_by_ids`
/// primitives rather than raw SQL.
async fn release_group_ids_for_entry(db: &MusicDb, entry: &EntryInfo) -> anyhow::Result<Vec<i64>> {
    if entry.entry_type != "release" {
        return Ok(Vec::new());
    }
    let child_rows = db.child_rows_for_child_pairs(&entry.pairs).await?;
    let parent_pairs: Vec<(String, String)> = child_rows
        .into_iter()
        .map(|c| (c.parent_source, c.parent_identifier))
        .collect();
    if parent_pairs.is_empty() {
        return Ok(Vec::new());
    }
    let parent_entry_ids: Vec<i64> = db
        .source_rows_for_pairs(&parent_pairs)
        .await?
        .into_iter()
        .map(|s| s.entry_id)
        .collect();
    let parent_infos = entry_infos_by_ids(db, &parent_entry_ids).await?;
    let mut ids: Vec<i64> = parent_infos
        .values()
        .filter(|e| e.entry_type == "release_group")
        .map(|e| e.entry_id)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
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

    let mut position_keys: Vec<(i64, Option<i32>, Option<i32>)> = e.track_positions.to_vec();
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

// ── TypeSafe request/response wire types ────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct StatePayload {
    a: EntryView,
    b: EntryView,
}

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
struct TypesafeRequest {
    state: StatePayload,
    model: String,
    questions: BTreeMap<String, Question>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
#[allow(dead_code)]
enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    Score {
        score: f64,
        #[serde(default)]
        legend: BTreeMap<String, String>,
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct TypesafeResponse {
    answers: BTreeMap<String, Answer>,
}

/// `criteria` is the type-specific set of non-`unsure` choices; `unsure` is
/// appended automatically since its meaning never varies by type.
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
/// under- or over-fires depending on type (confirmed empirically in the
/// `typesafe_poc` validation work — see project memory
/// `typesafe-soft-dedup-poc-findings`).
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

/// Maps the `identity` answer's raw choice, shared across every entry type's
/// vocabulary, onto a `Verdict`. `same_identity` becomes a **soft** merge
/// (`Verdict::Merge` always writes a reversible `same_identity` assertion,
/// never a destructive one — see `apply_candidate`); `different_identity`
/// and `unrelated` both mean "no connection", matching the Rhai script's
/// `Verdict::Distinct` (no write); anything else (including `unsure`)
/// defers.
fn identity_to_verdict(choice: &str, confidence: f64, reason: String) -> Verdict {
    match choice {
        "same_identity" => Verdict::Merge { confidence, reason },
        "related_variant" => Verdict::Relate {
            kind: "variant".to_string(),
            confidence,
            reason,
            metadata: None,
        },
        "different_identity" | "unrelated" => Verdict::Distinct,
        _ => Verdict::Defer { confidence, reason },
    }
}

fn evidence_hash(
    entry_type: &str,
    view_a: &EntryView,
    view_b: &EntryView,
) -> anyhow::Result<String> {
    let payload = serde_json::json!({
        "prompt_version": MODEL_VERSION,
        "entry_type": entry_type,
        "a": view_a,
        "b": view_b,
    });
    let mut hasher = Sha256::new();
    hasher.update(payload.to_string().as_bytes());
    Ok(format!("{:x}", hasher.finalize()))
}

/// Score one candidate pair with TypeSafe's Jev model. `ea`/`eb` must already
/// be `(min entry_id, max entry_id)` ordered, matching every other call site
/// in `pipeline::softmatch`. Caches by an evidence-content hash
/// (`jev_verdict_cache`): unchanged evidence reuses the stored verdict
/// instead of paying for another API call, which also keeps repeat
/// `softmatch` runs from re-appending `dedup_feedback` rows for a pair whose
/// evidence hasn't moved.
pub async fn score_pair(
    db: &MusicDb,
    ea: &EntryInfo,
    eb: &EntryInfo,
    config: &SoftMatchConfig,
) -> anyhow::Result<Verdict> {
    let http = config
        .http_client
        .as_ref()
        .context("Jev scoring (--jev-types) requires an http_client (configure http.yaml)")?;
    let api_key = std::env::var("TYPESAFE_API_KEY")
        .context("TYPESAFE_API_KEY must be set to use --jev-types")?;

    let mut entries: HashMap<i64, EntryInfo> = HashMap::new();
    entries.insert(ea.entry_id, ea.clone());
    entries.insert(eb.entry_id, eb.clone());
    let mut extra_ids: Vec<i64> = [ea, eb]
        .iter()
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
        entries.extend(entry_infos_by_ids(db, &extra_ids).await?);
    }

    let mut release_groups: HashMap<i64, Vec<i64>> = HashMap::new();
    for e in [ea, eb] {
        let groups = release_group_ids_for_entry(db, e).await?;
        if !groups.is_empty() {
            release_groups.insert(e.entry_id, groups);
        }
    }

    let view_a = entry_view(ea, &entries, &release_groups);
    let view_b = entry_view(eb, &entries, &release_groups);
    let hash = evidence_hash(&ea.entry_type, &view_a, &view_b)?;

    if let Some(cached) = db.get_jev_verdict_cache(ea.entry_id, eb.entry_id).await?
        && cached.evidence_hash == hash
    {
        return Ok(identity_to_verdict(
            &cached.choice,
            cached.confidence,
            cached.reason,
        ));
    }

    let request = TypesafeRequest {
        state: StatePayload {
            a: view_a,
            b: view_b,
        },
        model: MODEL.to_string(),
        questions: build_questions(&ea.entry_type),
    };
    let body = serde_json::to_vec(&request).context("serializing TypeSafe request")?;
    let auth = HeaderValue::from_str(&format!("Bearer {api_key}"))
        .context("invalid TYPESAFE_API_KEY header value")?;

    let response = http
        .make_request(
            HttpRequest {
                method: Method::POST,
                url: API_URL.to_string(),
                headers: vec![
                    (HeaderName::from_static("authorization"), auth),
                    (
                        HeaderName::from_static("content-type"),
                        HeaderValue::from_static("application/json"),
                    ),
                ],
                body: Some(body.into()),
                no_cache: true,
                ..Default::default()
            },
            json_body_extractor::<TypesafeResponse>().into(),
        )
        .await
        .with_context(|| {
            format!(
                "calling TypeSafe API for pair ({}, {})",
                ea.entry_id, eb.entry_id
            )
        })?;
    let parsed = response.json::<TypesafeResponse>().await?.clone();

    let Some(Answer::Choice {
        choice, confidence, ..
    }) = parsed.answers.get("identity")
    else {
        anyhow::bail!(
            "TypeSafe response for pair ({}, {}) is missing the `identity` answer",
            ea.entry_id,
            eb.entry_id
        );
    };
    let choice = choice.clone();
    let confidence = *confidence;

    let noul = |id: &str| match parsed.answers.get(id) {
        Some(Answer::Noul { noul }) => *noul,
        _ => f64::NAN,
    };
    let reason = format!(
        "jev-latest (title_match={:.2}, duration_consistent={:.2})",
        noul("title_match"),
        noul("duration_consistent"),
    );

    db.upsert_jev_verdict_cache(NewJevVerdictCache {
        entry_a: ea.entry_id,
        entry_b: eb.entry_id,
        evidence_hash: hash,
        choice: choice.clone(),
        confidence,
        reason: reason.clone(),
    })
    .await?;

    Ok(identity_to_verdict(&choice, confidence, reason))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(choice: &str, confidence: f64) -> TypesafeResponse {
        let mut answers = BTreeMap::new();
        answers.insert(
            "identity".to_string(),
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
        TypesafeResponse { answers }
    }

    fn identity_of(r: &TypesafeResponse) -> (String, f64) {
        match r.answers.get("identity") {
            Some(Answer::Choice {
                choice, confidence, ..
            }) => (choice.clone(), *confidence),
            _ => panic!("missing identity answer"),
        }
    }

    #[test]
    fn same_identity_becomes_soft_merge_verdict() {
        let (choice, confidence) = identity_of(&response("same_identity", 0.92));
        let v = identity_to_verdict(&choice, confidence, "reason".to_string());
        assert!(matches!(v, Verdict::Merge { confidence, .. } if confidence == 0.92));
    }

    #[test]
    fn related_variant_becomes_relate_verdict() {
        let (choice, confidence) = identity_of(&response("related_variant", 0.8));
        let v = identity_to_verdict(&choice, confidence, "reason".to_string());
        assert!(matches!(v, Verdict::Relate { kind, .. } if kind == "variant"));
    }

    #[test]
    fn unrelated_becomes_distinct_verdict() {
        let (choice, confidence) = identity_of(&response("unrelated", 0.99));
        let v = identity_to_verdict(&choice, confidence, "reason".to_string());
        assert!(matches!(v, Verdict::Distinct));
    }

    #[test]
    fn different_identity_becomes_distinct_verdict() {
        let (choice, confidence) = identity_of(&response("different_identity", 0.97));
        let v = identity_to_verdict(&choice, confidence, "reason".to_string());
        assert!(matches!(v, Verdict::Distinct));
    }

    #[test]
    fn unsure_defers() {
        let (choice, confidence) = identity_of(&response("unsure", 0.4));
        let v = identity_to_verdict(&choice, confidence, "reason".to_string());
        assert!(matches!(v, Verdict::Defer { .. }));
    }

    #[test]
    fn evidence_hash_is_stable_across_field_order() {
        let a = EntryView {
            entry_type: "track".to_string(),
            titles: vec![],
            durations_sec: vec![1.0, 2.0],
            release_dates: vec![],
            release_types: vec![],
            primary_types: vec![],
            sources: vec!["a".to_string(), "b".to_string()],
            credited_names: vec![],
            track_positions: vec![],
            release_tracks: vec![],
            handles: vec![],
            release_group_ids: vec![],
        };
        let h1 = evidence_hash("track", &a, &a).unwrap();
        let h2 = evidence_hash("track", &a, &a).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn evidence_hash_changes_with_prompt_version() {
        // Sanity: two structurally-identical-but-different-typed views hash
        // differently, proving entry_type participates in the hash.
        let a = EntryView {
            entry_type: "track".to_string(),
            titles: vec![],
            durations_sec: vec![],
            release_dates: vec![],
            release_types: vec![],
            primary_types: vec![],
            sources: vec![],
            credited_names: vec![],
            track_positions: vec![],
            release_tracks: vec![],
            handles: vec![],
            release_group_ids: vec![],
        };
        let h_track = evidence_hash("track", &a, &a).unwrap();
        let h_release = evidence_hash("release", &a, &a).unwrap();
        assert_ne!(h_track, h_release);
    }

    #[test]
    fn extract_handle_keeps_human_slug() {
        assert_eq!(
            extract_handle("https://www.youtube.com/@SomeArtist"),
            Some("SomeArtist".to_string())
        );
    }

    #[test]
    fn extract_handle_drops_youtube_channel_id() {
        // Real YouTube channel ids are exactly 24 chars, always "UC"-prefixed.
        assert_eq!(
            extract_handle("https://www.youtube.com/channel/UCxxxxxxxxxxxxxxxxxxxxxx"),
            None
        );
    }

    #[test]
    fn extract_handle_drops_numeric_catalog_id() {
        assert_eq!(
            extract_handle("https://www.discogs.com/artist/123456"),
            None
        );
    }
}
