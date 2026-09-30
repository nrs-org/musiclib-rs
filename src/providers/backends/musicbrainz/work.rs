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
use tracing::warn;

use crate::providers::{backends::musicbrainz::client::MusicBrainzClient, types::Error};

/// Above this count, the "originals" aren't the near-duplicate MB entries
/// for one physical original recording that this signal is meant to catch
/// (this module's doc comment) — they're every recording of a heavily
/// covered, often public-domain standard where most performances simply
/// never got explicitly tagged `cover` by a MusicBrainz editor. Observed for
/// real: a single work ("Silent Night") returning 700 "originals", each
/// imported as a full recursive entity by `recording.rs`'s caller — and
/// since some of *those* recordings are themselves tagged `cover` of the
/// same work, the same 700-item lookup can re-fire from multiple points in
/// the tree, without `state.claim()` catching it (Work lookups aren't
/// deduped — this module is deliberately not a modeled/tracked entity).
/// Capping bounds a single popular song from pulling in an entire genre's
/// discography.
const MAX_ORIGINALS: usize = 20;

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
                let mut originals: Vec<(String, String)> = resp
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
                if originals.len() > MAX_ORIGINALS {
                    warn!(
                        work_id = work_mbid,
                        count = originals.len(),
                        "musicbrainz: implausibly large 'original performance' set for a work \
                         (likely a heavily-covered/public-domain standard, not near-duplicate \
                         MB entries) — capping",
                    );
                    originals.truncate(MAX_ORIGINALS);
                }
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use http::Method;

    use super::{MAX_ORIGINALS, find_original_performances};
    use crate::{
        http::ResponseStatus, providers::backends::musicbrainz::client::MusicBrainzClient,
        test_utils::MockHttpClient,
    };

    fn work_api_url(mbid: &str) -> String {
        MusicBrainzClient::build_url(&format!("work/{mbid}"), &[("inc", "recording-rels")])
    }

    /// Real-world case this guards against: a heavily-covered, poorly-tagged
    /// standard ("Silent Night" returned 700) where most performances simply
    /// never got the `cover` attribute set by a MusicBrainz editor, rather
    /// than the few near-duplicate MB entries the underlying signal is meant
    /// to represent.
    #[tokio::test]
    async fn caps_an_implausibly_large_originals_set() -> anyhow::Result<()> {
        let mbid = "590e5567-c188-31f0-b7a8-a94e7e51c7b3";
        let raw_count = MAX_ORIGINALS + 50;
        let relations: Vec<serde_json::Value> = (0..raw_count)
            .map(|i| {
                serde_json::json!({
                    "type": "performance",
                    "target-type": "recording",
                    "attributes": [],
                    "recording": { "id": format!("rec-{i}"), "title": format!("Take {i}") },
                })
            })
            .collect();
        let body = serde_json::json!({ "relations": relations }).to_string();

        let mut http_client = MockHttpClient::new();
        http_client.add_route(
            Method::GET,
            &work_api_url(mbid),
            ResponseStatus::OK,
            Bytes::from(body),
        );
        let client = MusicBrainzClient::new_with_client(Arc::new(http_client), None)?;

        let originals = find_original_performances(&client, mbid).await?;
        assert_eq!(originals.len(), MAX_ORIGINALS);
        Ok(())
    }

    #[tokio::test]
    async fn leaves_a_small_originals_set_untouched() -> anyhow::Result<()> {
        let mbid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let relations: Vec<serde_json::Value> = (0..3)
            .map(|i| {
                serde_json::json!({
                    "type": "performance",
                    "target-type": "recording",
                    "attributes": [],
                    "recording": { "id": format!("rec-{i}"), "title": format!("Take {i}") },
                })
            })
            .collect();
        let body = serde_json::json!({ "relations": relations }).to_string();

        let mut http_client = MockHttpClient::new();
        http_client.add_route(
            Method::GET,
            &work_api_url(mbid),
            ResponseStatus::OK,
            Bytes::from(body),
        );
        let client = MusicBrainzClient::new_with_client(Arc::new(http_client), None)?;

        let originals = find_original_performances(&client, mbid).await?;
        assert_eq!(originals.len(), 3);
        Ok(())
    }
}
