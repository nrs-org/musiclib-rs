use std::{borrow::Cow, collections::HashSet, future::Future, sync::Arc};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tracing::warn;

use crate::providers::{
    backends::musicbrainz::{
        SOURCE,
        canonicalize::{artist_url, match_recording_url, recording_url},
        client::MusicBrainzClient,
        types::{EXTERNAL_TYPE_ARTIST, EXTERNAL_TYPE_RECORDING_ORIGINAL},
        url::{UrlResource, url_source_key},
        work,
    },
    types::{
        Alias, CachedChildSource, ChildRef, Contribution, EntityResult, EntrySpecificData,
        EntryType, Error,
    },
};

use super::artist_credit::{ArtistCredit, artist_credit_child_refs};

// --- API response types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RecordingResponse {
    pub id: String,
    pub title: String,
    /// Recording-level duration in milliseconds (aggregate across releases).
    pub length: Option<i64>,
    pub disambiguation: Option<String>,
    #[serde(rename = "artist-credit", default)]
    pub artist_credit: Vec<ArtistCredit>,
    #[serde(rename = "first-release-date")]
    pub first_release_date: Option<String>,
    #[serde(default)]
    pub relations: Vec<RecordingRelation>,
    /// Populated when `inc=releases+media` is requested.
    #[serde(default)]
    pub releases: Vec<RecordingRelease>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RecordingRelation {
    #[serde(rename = "type")]
    pub relation_type: String,
    #[serde(rename = "target-type")]
    pub target_type: String,
    #[serde(default)]
    pub attributes: Vec<String>,
    pub direction: Option<String>,
    pub artist: Option<RecordingRelationArtist>,
    pub url: Option<UrlResource>,
    /// Populated when `target_type == "recording"` (e.g. a `remix` relation).
    pub recording: Option<RecordingRelationTarget>,
    /// Populated when `target_type == "work"` (a `performance` relation —
    /// how MB models cover/live/karaoke/etc., see `work.rs`).
    pub work: Option<RecordingRelationTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RecordingRelationArtist {
    pub id: String,
    pub name: String,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RecordingRelationTarget {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RecordingRelease {
    #[serde(default)]
    pub media: Vec<RecordingMedium>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RecordingMedium {
    #[serde(default)]
    pub tracks: Vec<RecordingTrack>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RecordingTrack {
    pub length: Option<i64>,
}

// --- Constants ---

pub(crate) const PARTS: &str = "artist-credits+artist-rels+url-rels+recording-rels+work-rels";
/// No `+recordings`: MusicBrainz rejects it outright on this endpoint
/// ("recordings is not a valid inc parameter for the recording resource"),
/// unlike on a release lookup (`release.rs`'s own `PARTS`, where it *is*
/// valid) — this asymmetry is easy to miss since both look like ordinary
/// `inc` values. Without it, a track's medium never carries a `recording` id
/// back to disambiguate which of its tracks is *this* recording; see the
/// single-track-medium fallback in `get_recording` below.
pub(crate) const PARTS_WITH_RELEASES: &str =
    "artist-credits+artist-rels+url-rels+recording-rels+work-rels+releases+media";

// --- Public API ---

pub async fn get_recording_raw<T, F, R, E, FR>(
    client: &MusicBrainzClient,
    url: &str,
    callback: F,
) -> Result<R, E>
where
    T: std::any::Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
    F: FnOnce(&T, &str) -> FR,
    E: From<Error> + Send + 'static,
    FR: Future<Output = Result<R, E>> + Send + 'static,
{
    let mbid = match_recording_url(url).expect("Invalid MusicBrainz recording URL");
    client
        .get::<T, _, _, _, _>(&format!("recording/{mbid}"), &[("inc", PARTS)], |value| {
            callback(value, mbid)
        })
        .await
}

pub async fn get_recording(
    client: &MusicBrainzClient,
    url: &str,
    fetch_release_durations: bool,
    resolve_original_recordings: bool,
) -> Result<EntityResult<()>, Error> {
    let parts = if fetch_release_durations {
        PARTS_WITH_RELEASES
    } else {
        PARTS
    };
    let mbid = match_recording_url(url).expect("Invalid MusicBrainz recording URL");
    client
        .get::<RecordingResponse, _, _, Error, _>(
            &format!("recording/{mbid}"),
            &[("inc", parts)],
            |r| {
                let url = recording_url(mbid);
                let mut aliases = vec![Alias {
                    name: r.title.clone(),
                    source: SOURCE.into(),
                    primary: true,
                    ..Default::default()
                }];
                if let Some(dis) = &r.disambiguation
                    && !dis.is_empty()
                {
                    aliases.push(Alias {
                        name: format!("{} ({})", r.title, dis),
                        source: SOURCE.into(),
                        primary: false,
                        ..Default::default()
                    });
                }

                let mut artist_refs = artist_credit_child_refs(&r.artist_credit);

                for rel in &r.relations {
                    if rel.target_type != "artist" {
                        continue;
                    }
                    let Some(artist) = &rel.artist else { continue };
                    artist_refs.push(ChildRef {
                        entry_type: EntryType::Artist,
                        external_type: EXTERNAL_TYPE_ARTIST.into(),
                        sources: [(SOURCE.into(), HashSet::from([artist_url(&artist.id)]))].into(),
                        name: Some(artist.name.clone()),
                        contributions: vec![Contribution {
                            role: rel.relation_type.clone(),
                            main_artist: false,
                            source: SOURCE.into(),
                            extra: serde_json::Value::Null,
                        }],
                        ..Default::default()
                    });
                }

                let artist_children = CachedChildSource::from_children(artist_refs);

                let mut sources: crate::providers::types::ExternalSources =
                    [(SOURCE.into(), HashSet::from([url]))].into();
                for rel in r.relations.iter().filter(|r| r.target_type == "url") {
                    if let Some(u) = &rel.url {
                        let key = url_source_key(&u.resource);
                        sources
                            .0
                            .entry(key.into())
                            .or_default()
                            .insert(u.resource.clone());
                    }
                }

                // Collect per-release track lengths for this recording. A
                // recording lookup's `releases`/`media` inc has no way to
                // say which track on a medium is *this* recording (see
                // `PARTS_WITH_RELEASES`'s doc comment) — only single-track
                // media are unambiguous, so multi-track media are skipped
                // rather than risk attributing a co-track's length to this
                // recording.
                let mut duration_ms: Vec<i64> = r
                    .releases
                    .iter()
                    .flat_map(|rel| rel.media.iter())
                    .filter(|med| med.tracks.len() == 1)
                    .flat_map(|med| med.tracks.iter())
                    .filter_map(|t| t.length)
                    .collect();
                // Fall back to the recording-level aggregate when no track
                // lengths were found (e.g. when fetch_release_durations=false).
                if duration_ms.is_empty() {
                    duration_ms.extend(r.length);
                }
                duration_ms.sort_unstable();
                duration_ms.dedup();

                // Extract owned relation-trigger data up front — the
                // returned future is `'static` and can't borrow `r`.
                //
                // remix: a direct recording->recording relation. MB defines
                // the forward phrase ("remix of") from the remix's own
                // perspective, so `direction == "forward"` here means *this*
                // recording is the remix and the target is the original.
                let remix_targets: Vec<(String, String)> = r
                    .relations
                    .iter()
                    .filter(|rel| {
                        rel.target_type == "recording"
                            && rel.relation_type == "remix"
                            && rel.direction.as_deref() == Some("forward")
                    })
                    .filter_map(|rel| rel.recording.as_ref())
                    .map(|rec| (rec.id.clone(), rec.title.clone()))
                    .collect();
                // Every Work this recording performs, with whether that
                // performance is flagged `cover`. Arrangement/orchestration
                // is checked unconditionally per work below — it's a
                // Work<->Work fact, independent of this performance's own
                // attributes.
                let work_relations: Vec<(String, bool)> = r
                    .relations
                    .iter()
                    .filter(|rel| rel.target_type == "work" && rel.relation_type == "performance")
                    .filter_map(|rel| {
                        rel.work
                            .as_ref()
                            .map(|w| (w.id.clone(), rel.attributes.iter().any(|a| a == "cover")))
                    })
                    .collect();

                let client = client.clone();
                let extra = r.extra.clone();

                async move {
                    let mut original_refs: Vec<ChildRef> = Vec::new();
                    if resolve_original_recordings {
                        for (rec_id, title) in remix_targets {
                            original_refs.push(original_child_ref(&rec_id, &title, "remix"));
                        }
                        for (work_id, is_cover) in work_relations {
                            if is_cover {
                                match work::find_original_performances(&client, &work_id).await {
                                    Ok(originals) => {
                                        for (rec_id, title) in originals {
                                            original_refs
                                                .push(original_child_ref(&rec_id, &title, "cover"));
                                        }
                                    }
                                    Err(e) => warn!(
                                        "musicbrainz: cover-original lookup failed for work {work_id}: {e}"
                                    ),
                                }
                            }
                            match work::find_arrangement_source_work(&client, &work_id).await {
                                Ok(Some(source_work_id)) => {
                                    match work::find_original_performances(&client, &source_work_id)
                                        .await
                                    {
                                        Ok(originals) => {
                                            for (rec_id, title) in originals {
                                                original_refs.push(original_child_ref(
                                                    &rec_id,
                                                    &title,
                                                    "arrangement",
                                                ));
                                            }
                                        }
                                        Err(e) => warn!(
                                            "musicbrainz: arrangement-original lookup failed for work {source_work_id}: {e}"
                                        ),
                                    }
                                }
                                Ok(None) => {}
                                Err(e) => warn!(
                                    "musicbrainz: arrangement-source lookup failed for work {work_id}: {e}"
                                ),
                            }
                        }
                    }

                    let mut children = vec![Arc::new(artist_children)];
                    if !original_refs.is_empty() {
                        children.push(Arc::new(CachedChildSource::from_children(original_refs)));
                    }

                    Ok(EntityResult {
                        release_date: None,
                        sources,
                        extra,
                        specific_data: EntrySpecificData::Track {
                            duration_ms,
                            positions: Default::default(),
                        },
                        children,
                        aliases,
                    })
                }
            },
        )
        .await
}

/// Builds the synthesized `ChildRef` for an "original recording" discovered
/// via a cover/remix/arrangement relation. Tagged with
/// `EXTERNAL_TYPE_RECORDING_ORIGINAL` (rather than the plain recording type)
/// purely so `fetch_options` config authors can optionally special-case it —
/// dispatch in `Provider::fetch_entry` still treats it as an ordinary
/// recording.
fn original_child_ref(recording_mbid: &str, title: &str, kind: &'static str) -> ChildRef {
    ChildRef {
        entry_type: EntryType::Track,
        external_type: EXTERNAL_TYPE_RECORDING_ORIGINAL.into(),
        sources: [(
            SOURCE.into(),
            HashSet::from([recording_url(recording_mbid)]),
        )]
        .into(),
        name: Some(title.to_string()),
        original_relation_kind: Some(Cow::Borrowed(kind)),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use crate::providers::types::child_next;
    use std::sync::Arc;

    use http::Method;

    use crate::{
        providers::{
            backends::musicbrainz::{
                SOURCE,
                client::MusicBrainzClient,
                recording::{PARTS, RecordingResponse, get_recording},
                types::EXTERNAL_TYPE_RECORDING_ORIGINAL,
            },
            types::{ChildSource, EntrySpecificData, EntryType},
        },
        test_utils::MockHttpClient,
    };

    fn recording_api_url(mbid: &str) -> String {
        MusicBrainzClient::build_url(&format!("recording/{mbid}"), &[("inc", PARTS)])
    }

    fn recording_with_releases_api_url(mbid: &str) -> String {
        MusicBrainzClient::build_url(
            &format!("recording/{mbid}"),
            &[("inc", super::PARTS_WITH_RELEASES)],
        )
    }

    #[tokio::test]
    async fn test_get_recording() -> anyhow::Result<()> {
        let mbid = "1c75e623-32dc-4e2d-82d2-72d966cbe82b";
        let mut http_client = MockHttpClient::new();
        http_client.add_route_json::<RecordingResponse>(
            Method::GET,
            &recording_api_url(mbid),
            include_str!("./recording_watame_lullaby.json"),
        );

        let client = MusicBrainzClient::new_with_client(Arc::new(http_client), None)?;
        let rec = get_recording(
            &client,
            &format!("https://musicbrainz.org/recording/{mbid}"),
            false,
            true,
        )
        .await?;

        assert!(matches!(
            rec.specific_data,
            EntrySpecificData::Track {
                ref duration_ms,
                ..
            } if duration_ms == &vec![79000_i64]
        ));
        assert_eq!(
            rec.sources.get(SOURCE).unwrap(),
            &std::collections::HashSet::from([format!("https://musicbrainz.org/recording/{mbid}")])
        );
        assert_eq!(rec.aliases.len(), 1);
        assert_eq!(rec.aliases[0].name, "Watame Lullaby");
        assert!(rec.aliases[0].primary);

        // Artist child: Haruka Karibu. Also a regression check that
        // resolve_original_recordings=true doesn't push a spurious extra
        // child source when the fixture has no cover/remix/work relations.
        assert_eq!(rec.children.len(), 1);
        let mut cursor = rec.children[0].cursor();
        let (artist, _) = child_next(&mut cursor)
            .await?
            .expect("expected artist child");
        assert_eq!(artist.entry_type, EntryType::Artist);
        assert_eq!(artist.name.as_deref(), Some("Haruka Karibu"));
        assert_eq!(
            artist.sources.get(SOURCE).unwrap(),
            &std::collections::HashSet::from([
                "https://musicbrainz.org/artist/b7905b77-bb43-47f4-987f-15927df89b6b".to_string()
            ])
        );
        assert_eq!(artist.contributions.len(), 1);
        assert!(artist.contributions[0].main_artist);

        Ok(())
    }

    /// Regression test for the `@ShirakamiFubuki` channel import where every
    /// MusicBrainz recording lookup failed with a 400: `PARTS_WITH_RELEASES`
    /// used to include an `inc=recordings` MusicBrainz rejects outright on
    /// the recording resource. This both pins the corrected `inc=` value (the
    /// mock only matches that exact URL, so a regression here would 404, not
    /// silently pass) and checks the single-track-medium fallback picks up
    /// the unambiguous release's length while skipping the two-track one.
    #[tokio::test]
    async fn test_get_recording_with_release_durations() -> anyhow::Result<()> {
        let mbid = "2b6b1d1e-9c1a-4e1f-8f0a-000000000001";
        let mut http_client = MockHttpClient::new();
        http_client.add_route_json::<RecordingResponse>(
            Method::GET,
            &recording_with_releases_api_url(mbid),
            include_str!("./recording_beautiful_circle_with_releases.json"),
        );

        let client = MusicBrainzClient::new_with_client(Arc::new(http_client), None)?;
        let rec = get_recording(
            &client,
            &format!("https://musicbrainz.org/recording/{mbid}"),
            true,
            true,
        )
        .await?;

        assert!(matches!(
            rec.specific_data,
            EntrySpecificData::Track { ref duration_ms, .. } if duration_ms == &vec![208000_i64]
        ));

        Ok(())
    }

    fn work_api_url(mbid: &str) -> String {
        MusicBrainzClient::build_url(&format!("work/{mbid}"), &[("inc", "recording-rels")])
    }

    async fn collect_child_names(
        source: &Arc<crate::providers::types::CachedChildSource<()>>,
    ) -> anyhow::Result<Vec<String>> {
        let mut cursor = source.owned_cursor();
        let mut names = Vec::new();
        while let Some((child, _)) = child_next(&mut cursor).await? {
            names.push(child.name.clone().unwrap_or_default());
        }
        Ok(names)
    }

    /// A cover is not a recording->recording relation — it's
    /// `recording --performance(attributes:["cover"])--> Work`, and the Work
    /// (`work_king.json`) has two untagged ("original") performances among
    /// several tagged cover/live ones. Both untagged recordings should be
    /// imported as `original` children, each tagged `original_relation_kind:
    /// Some("cover")`; the tagged siblings (the other cover, the live
    /// version) must NOT be pulled in.
    #[tokio::test]
    async fn test_cover_relation_imports_untagged_originals() -> anyhow::Result<()> {
        let mbid = "cfe0e8fb-2e0d-4a8c-81bb-5448cbc3d391";
        let work_mbid = "7b11aea3-c733-42a5-a8cb-de0becb5f6ba";
        let mut http_client = MockHttpClient::new();
        http_client.add_route_json::<RecordingResponse>(
            Method::GET,
            &recording_api_url(mbid),
            include_str!("./recording_king_cover.json"),
        );
        http_client.add_route_json::<serde_json::Value>(
            Method::GET,
            &work_api_url(work_mbid),
            include_str!("./work_king.json"),
        );

        let client = MusicBrainzClient::new_with_client(Arc::new(http_client), None)?;
        let rec = get_recording(
            &client,
            &format!("https://musicbrainz.org/recording/{mbid}"),
            false,
            true,
        )
        .await?;

        // [0] = artist children (empty here), [1] = original-recording children.
        assert_eq!(rec.children.len(), 2);
        let mut names = collect_child_names(&rec.children[1]).await?;
        names.sort();
        assert_eq!(names, vec!["KING".to_string(), "KING".to_string()]);

        let mut cursor = rec.children[1].owned_cursor();
        let mut kinds = Vec::new();
        while let Some((child, _)) = child_next(&mut cursor).await? {
            kinds.push(child.original_relation_kind.clone().map(|c| c.to_string()));
            assert_eq!(
                child.external_type.as_ref(),
                EXTERNAL_TYPE_RECORDING_ORIGINAL
            );
        }
        assert_eq!(
            kinds,
            vec![Some("cover".to_string()), Some("cover".to_string())]
        );

        Ok(())
    }

    /// `remix` is a direct recording->recording relation — no Work lookup
    /// needed, so this must resolve with only the base recording HTTP call
    /// (the mock has no other routes registered; an unexpected extra call
    /// would fail the test).
    #[tokio::test]
    async fn test_remix_relation_imports_original() -> anyhow::Result<()> {
        let mbid = "33333333-3333-3333-3333-333333333333";
        let mut http_client = MockHttpClient::new();
        http_client.add_route_json::<RecordingResponse>(
            Method::GET,
            &recording_api_url(mbid),
            include_str!("./recording_remix_example.json"),
        );

        let client = MusicBrainzClient::new_with_client(Arc::new(http_client), None)?;
        let rec = get_recording(
            &client,
            &format!("https://musicbrainz.org/recording/{mbid}"),
            false,
            true,
        )
        .await?;

        assert_eq!(rec.children.len(), 2);
        let names = collect_child_names(&rec.children[1]).await?;
        assert_eq!(names, vec!["Example Song".to_string()]);

        let mut cursor = rec.children[1].owned_cursor();
        let (child, _) = child_next(&mut cursor)
            .await?
            .expect("expected one original child");
        assert_eq!(child.original_relation_kind.as_deref(), Some("remix"));
        assert_eq!(
            child.sources.get(SOURCE).unwrap(),
            &std::collections::HashSet::from([
                "https://musicbrainz.org/recording/55555555-5555-5555-5555-555555555555"
                    .to_string()
            ])
        );

        Ok(())
    }

    /// `resolve_original_recordings: false` must skip both the direct-relation
    /// and Work-lookup paths entirely — no extra child source, no extra HTTP
    /// call (the mock has no `work/` route registered).
    #[tokio::test]
    async fn test_resolve_original_recordings_false_skips_lookup() -> anyhow::Result<()> {
        let mbid = "cfe0e8fb-2e0d-4a8c-81bb-5448cbc3d391";
        let mut http_client = MockHttpClient::new();
        http_client.add_route_json::<RecordingResponse>(
            Method::GET,
            &recording_api_url(mbid),
            include_str!("./recording_king_cover.json"),
        );

        let client = MusicBrainzClient::new_with_client(Arc::new(http_client), None)?;
        let rec = get_recording(
            &client,
            &format!("https://musicbrainz.org/recording/{mbid}"),
            false,
            false,
        )
        .await?;

        assert_eq!(rec.children.len(), 1);

        Ok(())
    }
}
