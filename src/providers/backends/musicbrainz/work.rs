//! MusicBrainz Work lookups. Work is *not* a modeled entity in musiclib-rs
//! (no `EntryType::Work`, never a `ChildRef`, never stored) — it exists here
//! purely as a transient hop used to resolve "the original recording of a
//! cover/arrangement", per the design in `docs/plan-*.md`.
//!
//! MB doesn't flag a single recording as "the" canonical original performance
//! of a Work. The best available signal is a `performance` relation with an
//! **empty** `attributes` array (no `cover`/`live`/`karaoke`/etc. tag) — this
//! can return zero, one, or more than one recording (e.g. near-duplicate MB
//! entries for the same physical original), and callers should treat all of
//! them as valid originals rather than trying to pick just one.

use serde::{Deserialize, Serialize};

use crate::providers::{backends::musicbrainz::client::MusicBrainzClient, types::Error};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkResponse {
    #[serde(default)]
    relations: Vec<WorkRelation>,
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
    recording: Option<WorkRelationTarget>,
    work: Option<WorkRelationTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkRelationTarget {
    id: String,
    title: String,
}

/// Recordings that perform `work_mbid` with no version-qualifying attribute
/// (cover/live/karaoke/instrumental/...) — the best available "original
/// recording" signal MB exposes. Returns `(recording mbid, title)` pairs;
/// empty on 404 (a dangling/removed work id) rather than erroring, matching
/// `isrc::lookup_isrc`'s not-found handling.
pub(crate) async fn find_original_performances(
    client: &MusicBrainzClient,
    work_mbid: &str,
) -> Result<Vec<(String, String)>, Error> {
    let result = client
        .get::<WorkResponse, _, _, Error, _>(
            &format!("work/{work_mbid}"),
            &[("inc", "recording-rels")],
            |resp| {
                let originals: Vec<(String, String)> = resp
                    .relations
                    .iter()
                    .filter(|r| {
                        r.target_type == "recording"
                            && r.relation_type == "performance"
                            && r.attributes.is_empty()
                    })
                    .filter_map(|r| r.recording.as_ref())
                    .map(|rec| (rec.id.clone(), rec.title.clone()))
                    .collect();
                async move { Ok(originals) }
            },
        )
        .await;
    match result {
        Ok(originals) => Ok(originals),
        Err(Error::NotFound(_)) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// If `work_mbid` is an Arrangement/Orchestration *of* another work (i.e.
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
pub(crate) async fn find_arrangement_source_work(
    client: &MusicBrainzClient,
    work_mbid: &str,
) -> Result<Option<String>, Error> {
    let result = client
        .get::<WorkResponse, _, _, Error, _>(
            &format!("work/{work_mbid}"),
            &[("inc", "work-rels")],
            |resp| {
                let source = resp
                    .relations
                    .iter()
                    .find(|r| {
                        r.target_type == "work"
                            && (r.relation_type == "arrangement"
                                || r.relation_type == "orchestration")
                            && r.direction.as_deref() == Some("backward")
                    })
                    .and_then(|r| r.work.as_ref())
                    .map(|w| w.id.clone());
                async move { Ok(source) }
            },
        )
        .await;
    match result {
        Ok(source) => Ok(source),
        Err(Error::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}
