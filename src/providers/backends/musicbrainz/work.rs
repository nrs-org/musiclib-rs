//! MusicBrainz Work lookups. Work is *not* a modeled entity in musiclib-rs
//! (no `EntryType::Work`, never a `ChildRef`, never stored as an entry) — it
//! provides the writing credits for a recording's performed works and helps
//! resolve original recordings of covers/arrangements. Recordings keep work
//! MBIDs grouped into songs (`EntrySpecificData::Track::songs`) and each
//! projected artist credit keeps its work and performance context.
//!
//! MB doesn't flag a single recording as "the" canonical original performance
//! of a Work. The best available signal is a `performance` relation with an
//! **empty** `attributes` array (no `cover`/`live`/`karaoke`/etc. tag), but a
//! popular work has dozens of those: DJ-mix fragments, music videos,
//! compilation re-issues, named remixes that nobody tagged. Linking a cover to
//! all of them drags every one into the cover's edition group, so
//! `find_original_anchors` picks the one recording that best stands for the
//! original (see its doc comment).

use std::collections::HashMap;

use crate::providers::{
    backends::musicbrainz::{
        SOURCE,
        canonicalize::artist_url,
        client::MusicBrainzClient,
        recording::{RecordingRelation, RecordingRelationArtist},
        types::EXTERNAL_TYPE_ARTIST,
    },
    types::{ChildRef, Contribution, EntryType, Error},
};
use serde::{Deserialize, Serialize};

/// A single lookup supplies both artist credits and work-version links.
/// The caller reuses it for grouping and arrangement discovery, rather
/// than looking up the same work separately for each purpose.
pub(crate) const CREDIT_PARTS: &str = "artist-rels+work-rels";

/// Page size for the `recording?work=` browse (MB's maximum).
const BROWSE_PAGE_SIZE: usize = 100;
/// Upper bound on browse pages per work. Recordings past this are ranked as
/// undated — only heavily-covered standards ("Silent Night": 700+) get here.
const MAX_BROWSE_PAGES: usize = 5;
/// Two anchor candidates within this many ms, with the same title and first
/// release date, are taken as duplicate MB entries for one master.
const DUPLICATE_LENGTH_TOLERANCE_MS: i64 = 2_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkResponse {
    title: String,
    #[serde(default)]
    relations: Vec<WorkRelation>,
    #[serde(flatten)]
    extra: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkRelation {
    #[serde(rename = "type")]
    relation_type: String,
    #[serde(rename = "target-type")]
    target_type: String,
    #[serde(default)]
    attributes: Vec<String>,
    direction: Option<String>,
    recording: Option<WorkRelationRecording>,
    work: Option<WorkRelationTarget>,
    artist: Option<RecordingRelationArtist>,
    #[serde(flatten)]
    extra: serde_json::Value,
}

/// Fetch each distinct performed work once. A removed/dangling work is
/// absent; other failures abort the recording fetch so a re-import cannot
/// silently replace previously known writing credits with an empty list.
pub(crate) async fn lookup_works(
    client: &MusicBrainzClient,
    works: &[String],
) -> Result<HashMap<String, WorkResponse>, Error> {
    let mut found = HashMap::new();
    let unique: std::collections::BTreeSet<_> = works.iter().collect();
    for id in unique {
        let result = client
            .get::<WorkResponse, _, _, Error, _>(
                &format!("work/{id}"),
                &[("inc", CREDIT_PARTS)],
                |work| {
                    let work = work.clone();
                    async move { Ok(work) }
                },
            )
            .await;
        match result {
            Ok(work) => {
                found.insert(id.clone(), work);
            }
            Err(Error::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(found)
}

impl WorkResponse {
    /// Project only this performed work's own artist relations. Version,
    /// translation, arrangement, and recording links are not credit
    /// inheritance paths. Keep generic writers/translators as their stated
    /// roles rather than guessing which NRS role or share they deserve.
    pub(crate) fn artist_credits(
        &self,
        work_id: &str,
        performance: &RecordingRelation,
    ) -> Vec<ChildRef> {
        self.relations
            .iter()
            .filter(|rel| rel.target_type == "artist")
            .filter_map(|rel| {
                let artist = rel.artist.as_ref()?;
                let mut extra = rel.extra.clone();
                extra["attributes"] = serde_json::json!(rel.attributes);
                extra["direction"] = serde_json::json!(rel.direction);
                extra["credited_name"] = serde_json::json!(
                    rel.extra["target-credit"]
                        .as_str()
                        .filter(|name| !name.is_empty())
                        .unwrap_or(&artist.name)
                );
                extra["scope"] = serde_json::json!("work");
                extra["work"] = serde_json::json!({
                    "id": work_id,
                    "title": self.title,
                    "url": format!("https://musicbrainz.org/work/{work_id}"),
                    "language": self.extra["language"],
                    "languages": self.extra["languages"],
                });
                // Retain relationship dates, partial/cover/instrumental/
                // medley flags, credited-as names and other qualifiers.
                let mut context = performance.extra.clone();
                context["type"] = serde_json::json!(performance.relation_type);
                context["direction"] = serde_json::json!(performance.direction);
                context["attributes"] = serde_json::json!(performance.attributes);
                extra["performance"] = context;
                Some(ChildRef {
                    entry_type: EntryType::Artist,
                    external_type: EXTERNAL_TYPE_ARTIST.into(),
                    sources: [(
                        SOURCE.into(),
                        std::collections::HashSet::from([artist_url(&artist.id)]),
                    )]
                    .into(),
                    name: Some(artist.name.clone()),
                    contributions: vec![Contribution {
                        role: rel.relation_type.clone(),
                        main_artist: false,
                        source: SOURCE.into(),
                        extra,
                    }],
                    ..Default::default()
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkRelationTarget {
    id: String,
    title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkRelationRecording {
    id: String,
    title: String,
    #[serde(default)]
    disambiguation: String,
    #[serde(default)]
    video: bool,
    length: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecordingBrowse {
    #[serde(rename = "recording-count", default)]
    recording_count: usize,
    #[serde(default)]
    recordings: Vec<BrowsedRecording>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BrowsedRecording {
    id: String,
    #[serde(rename = "first-release-date")]
    first_release_date: Option<String>,
    #[serde(default)]
    isrcs: Vec<String>,
}

/// Disambiguation fragments that mark a recording as a derived or partial
/// take rather than the original (matched against the lowercased text with
/// Unicode hyphens folded to `-`). "radio edit" is deliberately absent: it's
/// often how MB labels the single version itself.
const NON_ORIGINAL_DISAMBIGUATIONS: &[&str] = &[
    "dj-mix",
    "dj mix",
    "mixed",
    "megamix",
    "nonstop",
    "part of",
    "video",
    "voiceover",
    "live",
    "karaoke",
    "instrumental",
    "remix",
    "demo",
    "medley",
    "atmos",
];

fn fold(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| match c {
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            c => c,
        })
        .collect()
}

fn looks_original(rec: &WorkRelationRecording) -> bool {
    let dis = fold(&rec.disambiguation);
    !rec.video && !NON_ORIGINAL_DISAMBIGUATIONS.iter().any(|d| dis.contains(d))
}

/// The anchor recording(s) standing for `work_mbid`'s original performance,
/// as `(recording mbid, title)` pairs; empty on 404 (a dangling/removed work
/// id) rather than erroring, matching `isrc::lookup_isrc`'s not-found handling.
///
/// Among untagged performances (see the module doc):
/// 1. drop videos and recordings whose disambiguation marks them derived
///    (`NON_ORIGINAL_DISAMBIGUATIONS`), unless that drops all of them;
/// 2. prefer a title equal to the work's own title, which rules out named
///    versions like "Wannabe (Motiv 8 mix)";
/// 3. rank by earliest first release date, then having an ISRC, then MB's
///    order, using a `recording?work=` browse (the work lookup carries
///    neither); the browse is skipped when one candidate is left.
///
/// Returns the top candidate, plus any that look like a duplicate MB entry
/// for the same master (same title and date, length within
/// `DUPLICATE_LENGTH_TOLERANCE_MS`).
pub(crate) async fn find_original_anchors(
    client: &MusicBrainzClient,
    work_mbid: &str,
) -> Result<Vec<(String, String)>, Error> {
    let work = client
        .get::<WorkResponse, _, _, Error, _>(
            &format!("work/{work_mbid}"),
            &[("inc", "recording-rels")],
            |resp| {
                let resp = resp.clone();
                async move { Ok(resp) }
            },
        )
        .await;
    let work = match work {
        Ok(work) => work,
        Err(Error::NotFound(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let untagged: Vec<WorkRelationRecording> = work
        .relations
        .into_iter()
        .filter(|r| {
            r.target_type == "recording"
                && r.relation_type == "performance"
                && r.attributes.is_empty()
        })
        .filter_map(|r| r.recording)
        .collect();
    let mut candidates: Vec<WorkRelationRecording> = untagged
        .iter()
        .filter(|r| looks_original(r))
        .cloned()
        .collect();
    if candidates.is_empty() {
        candidates = untagged;
    }
    let work_title = fold(work.title.trim());
    if candidates
        .iter()
        .any(|r| fold(r.title.trim()) == work_title)
    {
        candidates.retain(|r| fold(r.title.trim()) == work_title);
    }
    if candidates.len() <= 1 {
        return Ok(candidates.into_iter().map(|r| (r.id, r.title)).collect());
    }

    let facts = browse_release_facts(client, work_mbid).await?;
    let key = |(index, rec): &(usize, &WorkRelationRecording)| {
        let (date, has_isrc) = facts
            .get(&rec.id)
            .map(|(date, has_isrc)| (date.clone(), *has_isrc))
            .unwrap_or((None, false));
        // `None` dates sort last; MB dates are ISO prefixes ("1996",
        // "1996-07", "1996-07-03"), so string order is chronological.
        (date.is_none(), date, !has_isrc, *index)
    };
    let mut ranked: Vec<(usize, &WorkRelationRecording)> = candidates.iter().enumerate().collect();
    ranked.sort_by_key(key);

    let (_, top) = ranked[0];
    let top_date = facts.get(&top.id).and_then(|(date, _)| date.clone());
    let mut anchors = vec![(top.id.clone(), top.title.clone())];
    if top_date.is_some() {
        for (_, rec) in &ranked[1..] {
            let same_date = facts.get(&rec.id).and_then(|(date, _)| date.clone()) == top_date;
            let close_length = match (top.length, rec.length) {
                (Some(a), Some(b)) => (a - b).abs() <= DUPLICATE_LENGTH_TOLERANCE_MS,
                _ => false,
            };
            if same_date && close_length && fold(&rec.title) == fold(&top.title) {
                anchors.push((rec.id.clone(), rec.title.clone()));
            }
        }
    }
    Ok(anchors)
}

/// `recording mbid -> (first release date, has an ISRC)` for every recording
/// of `work_mbid`, up to `MAX_BROWSE_PAGES` pages.
async fn browse_release_facts(
    client: &MusicBrainzClient,
    work_mbid: &str,
) -> Result<HashMap<String, (Option<String>, bool)>, Error> {
    let mut facts = HashMap::new();
    let limit = BROWSE_PAGE_SIZE.to_string();
    for page in 0..MAX_BROWSE_PAGES {
        let offset = (page * BROWSE_PAGE_SIZE).to_string();
        let mut params = vec![
            ("work", work_mbid),
            ("inc", "isrcs"),
            ("limit", limit.as_str()),
        ];
        if page > 0 {
            params.push(("offset", offset.as_str()));
        }
        let browse = client
            .get::<RecordingBrowse, _, _, Error, _>("recording", &params, |resp| {
                let resp = resp.clone();
                async move { Ok(resp) }
            })
            .await?;
        let seen = browse.recordings.len();
        for rec in browse.recordings {
            let date = rec.first_release_date.filter(|d| !d.is_empty());
            facts.insert(rec.id, (date, !rec.isrcs.is_empty()));
        }
        if seen < BROWSE_PAGE_SIZE || (page + 1) * BROWSE_PAGE_SIZE >= browse.recording_count {
            break;
        }
    }
    Ok(facts)
}

/// Work-work relation types under which two works are versions of one song
/// (a translated lyric, a revision, an arrangement), as opposed to
/// `medley`/`parts`, which join different songs.
const SAME_SONG_WORK_RELATIONS: &[&str] = &[
    "other version",
    "based on",
    "revision of",
    "translation",
    "arrangement",
    "orchestration",
];

/// Groups the works one recording performs into songs: two of them are one
/// song when either links to the other by a `SAME_SONG_WORK_RELATIONS`
/// relation. MB commonly credits a translated cover with both the original
/// work and its translation, which is still one song, not a medley. A work
/// whose lookup returned 404 stays a song of its own. Each song's works are
/// sorted, and songs are sorted by their first work.
pub(crate) fn group_into_songs(
    works: &[String],
    details: &HashMap<String, WorkResponse>,
) -> Vec<Vec<String>> {
    let mut song_of: Vec<usize> = (0..works.len()).collect();
    fn root(song_of: &mut [usize], mut i: usize) -> usize {
        while song_of[i] != i {
            song_of[i] = song_of[song_of[i]];
            i = song_of[i];
        }
        i
    }
    if works.len() > 1 {
        for (i, work) in works.iter().enumerate() {
            let Some(detail) = details.get(work) else {
                continue;
            };
            let linked: Vec<_> = detail
                .relations
                .iter()
                .filter(|r| {
                    r.target_type == "work"
                        && SAME_SONG_WORK_RELATIONS.contains(&r.relation_type.as_str())
                })
                .filter_map(|r| r.work.as_ref())
                .map(|w| &w.id)
                .collect();
            for (j, other) in works.iter().enumerate() {
                if linked.contains(&other) {
                    let (a, b) = (root(&mut song_of, i), root(&mut song_of, j));
                    song_of[a.max(b)] = a.min(b);
                }
            }
        }
    }
    let mut songs: std::collections::BTreeMap<usize, Vec<String>> = Default::default();
    for (i, work) in works.iter().enumerate() {
        let r = root(&mut song_of, i);
        songs.entry(r).or_default().push(work.clone());
    }
    let mut songs: Vec<Vec<String>> = songs
        .into_values()
        .map(|mut song| {
            song.sort_unstable();
            song.dedup();
            song
        })
        .collect();
    songs.sort_unstable();
    songs
}

/// If this work is an Arrangement/Orchestration *of* another work (i.e.
/// this work is the derived side), returns that source work's mbid.
///
/// Direction matters and is *not* uniform across relation types — MB defines
/// a forward/reverse phrase pair per relationship type independently:
/// - Arrangement: forward phrase read from the original ("arrangements")
///   names the arrangement as target; reverse phrase read from the
///   arrangement ("arrangement of") names the original as target. So the
///   current work is the *derived* side exactly when `direction == "backward"`.
/// - Orchestration: same shape (forward "orchestrations" / reverse
///   "orchestration of"), same rule.
pub(crate) fn find_arrangement_source_work(work: &WorkResponse) -> Option<String> {
    work.relations
        .iter()
        .find(|r| {
            r.target_type == "work"
                && matches!(r.relation_type.as_str(), "arrangement" | "orchestration")
                && r.direction.as_deref() == Some("backward")
        })
        .and_then(|r| r.work.as_ref())
        .map(|w| w.id.clone())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use http::Method;
    use serde_json::{Value, json};

    use super::{CREDIT_PARTS, find_original_anchors, group_into_songs, lookup_works};
    use crate::{
        http::ResponseStatus, providers::backends::musicbrainz::client::MusicBrainzClient,
        test_utils::MockHttpClient,
    };

    fn work_api_url(mbid: &str) -> String {
        MusicBrainzClient::build_url(&format!("work/{mbid}"), &[("inc", "recording-rels")])
    }

    fn work_rels_api_url(mbid: &str) -> String {
        MusicBrainzClient::build_url(&format!("work/{mbid}"), &[("inc", CREDIT_PARTS)])
    }

    fn browse_api_url(mbid: &str) -> String {
        MusicBrainzClient::build_url(
            "recording",
            &[("work", mbid), ("inc", "isrcs"), ("limit", "100")],
        )
    }

    fn performance(id: &str, title: &str, attributes: &[&str], extra: Value) -> Value {
        let mut recording = json!({ "id": id, "title": title });
        if let (Some(rec), Some(extra)) = (recording.as_object_mut(), extra.as_object()) {
            rec.extend(extra.clone());
        }
        json!({
            "type": "performance",
            "target-type": "recording",
            "direction": "backward",
            "attributes": attributes,
            "recording": recording,
        })
    }

    fn client_with(routes: Vec<(String, String)>) -> anyhow::Result<MusicBrainzClient> {
        let mut http_client = MockHttpClient::new();
        for (url, body) in routes {
            http_client.add_route(Method::GET, &url, ResponseStatus::OK, Bytes::from(body));
        }
        Ok(MusicBrainzClient::new_with_client(
            Arc::new(http_client),
            None,
        )?)
    }

    fn ids(anchors: &[(String, String)]) -> Vec<&str> {
        anchors.iter().map(|(id, _)| id.as_str()).collect()
    }

    /// Real data: "KING" has two untagged performances, Kanaria's 2020
    /// original and a 2022 recording. Only the earliest is the anchor.
    #[tokio::test]
    async fn picks_the_earliest_untagged_performance() -> anyhow::Result<()> {
        let mbid = "7b11aea3-c733-42a5-a8cb-de0becb5f6ba";
        let client = client_with(vec![
            (
                work_api_url(mbid),
                include_str!("./work_king.json").to_string(),
            ),
            (
                browse_api_url(mbid),
                include_str!("./work_king_recordings.json").to_string(),
            ),
        ])?;

        let anchors = find_original_anchors(&client, mbid).await?;
        assert_eq!(ids(&anchors), vec!["6472bbab-cec1-41b9-b19f-12232639f0dd"]);
        Ok(())
    }

    /// Shaped like "Wannabe": most untagged performances are DJ-mix
    /// fragments, videos and named mixes. The dated ISRC single wins, and an
    /// MB duplicate of it (same date, title and length) comes along.
    #[tokio::test]
    async fn skips_derived_takes_and_keeps_master_duplicates() -> anyhow::Result<()> {
        let mbid = "d2520d0d-ed03-3dd8-8607-0724ee6d1a2e";
        let work = json!({
            "title": "Wannabe",
            "relations": [
                performance("dj", "Wannabe", &[], json!({ "disambiguation": "part of \u{201c}Zipmania DX\u{201d} DJ\u{2010}mix", "length": 155000 })),
                performance("mv", "Wannabe", &[], json!({ "video": true, "length": 238000 })),
                performance("mix", "Wannabe (Motiv 8 mix)", &[], json!({ "length": 229000 })),
                performance("comp", "Wannabe", &[], json!({ "length": 211773 })),
                performance("single", "Wannabe", &[], json!({ "disambiguation": "radio edit", "length": 173000 })),
                performance("dup", "Wannabe", &[], json!({ "length": 173026 })),
                performance("cover", "Wannabe", &["cover"], json!({ "length": 172000 })),
            ],
        });
        let browse = json!({
            "recording-count": 4,
            "recordings": [
                { "id": "comp", "first-release-date": "1997", "isrcs": [] },
                { "id": "single", "first-release-date": "1996-07-03", "isrcs": ["GBAAA9600008"] },
                { "id": "dup", "first-release-date": "1996-07-03", "isrcs": [] },
                { "id": "dj", "first-release-date": "1995", "isrcs": [] },
            ],
        });
        let client = client_with(vec![
            (work_api_url(mbid), work.to_string()),
            (browse_api_url(mbid), browse.to_string()),
        ])?;

        let anchors = find_original_anchors(&client, mbid).await?;
        assert_eq!(ids(&anchors), vec!["single", "dup"]);
        Ok(())
    }

    /// One candidate left after filtering needs no browse (the mock has no
    /// browse route, so a browse call would fail the test).
    #[tokio::test]
    async fn single_candidate_skips_the_browse() -> anyhow::Result<()> {
        let mbid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let work = json!({
            "title": "Song",
            "relations": [
                performance("live", "Song", &[], json!({ "disambiguation": "live at Budokan" })),
                performance("orig", "Song", &[], json!({})),
                performance("cover", "Song", &["cover"], json!({})),
            ],
        });
        let client = client_with(vec![(work_api_url(mbid), work.to_string())])?;

        let anchors = find_original_anchors(&client, mbid).await?;
        assert_eq!(ids(&anchors), vec!["orig"]);
        Ok(())
    }

    /// When every untagged performance looks derived, fall back to ranking
    /// all of them rather than returning nothing.
    #[tokio::test]
    async fn falls_back_when_every_candidate_looks_derived() -> anyhow::Result<()> {
        let mbid = "bbbbbbbb-bbbb-cccc-dddd-eeeeeeeeeeee";
        let work = json!({
            "title": "Song",
            "relations": [
                performance("mv2", "Song", &[], json!({ "video": true })),
                performance("mv1", "Song", &[], json!({ "video": true })),
            ],
        });
        let browse = json!({
            "recording-count": 2,
            "recordings": [
                { "id": "mv2", "first-release-date": "2021-05-01", "isrcs": [] },
                { "id": "mv1", "first-release-date": "2020-01-01", "isrcs": [] },
            ],
        });
        let client = client_with(vec![
            (work_api_url(mbid), work.to_string()),
            (browse_api_url(mbid), browse.to_string()),
        ])?;

        let anchors = find_original_anchors(&client, mbid).await?;
        assert_eq!(ids(&anchors), vec!["mv1"]);
        Ok(())
    }

    fn work_rel(kind: &str, direction: &str, id: &str) -> Value {
        json!({
            "type": kind,
            "target-type": "work",
            "direction": direction,
            "attributes": [],
            "work": { "id": id, "title": id },
        })
    }

    /// Shaped like "Say So (Japanese Version)" + a medley partner: the
    /// original and its translation are one song, linked by `other version`
    /// from either side; a work joined only by `medley` stays its own song.
    #[tokio::test]
    async fn groups_version_linked_works_into_one_song() -> anyhow::Result<()> {
        let client = client_with(vec![
            (
                work_rels_api_url("orig"),
                json!({ "title": "Say So", "relations": [
                    work_rel("other version", "forward", "ja"),
                    work_rel("medley", "backward", "other"),
                ] })
                .to_string(),
            ),
            (
                work_rels_api_url("ja"),
                json!({ "title": "Say So (Japanese Version)", "relations": [] }).to_string(),
            ),
            (
                work_rels_api_url("other"),
                json!({ "title": "Other", "relations": [] }).to_string(),
            ),
        ])?;

        let works = ["ja", "orig", "other"].map(String::from);
        let details = lookup_works(&client, &works).await?;
        let songs = group_into_songs(&works, &details);
        assert_eq!(
            songs,
            vec![
                vec!["ja".to_string(), "orig".to_string()],
                vec!["other".to_string()],
            ]
        );
        Ok(())
    }
}
