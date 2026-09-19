use std::{collections::HashSet, future::Future, sync::Arc};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::providers::{
    backends::musicbrainz::{
        SOURCE,
        canonicalize::{artist_url, match_recording_url, recording_url},
        client::MusicBrainzClient,
        types::EXTERNAL_TYPE_ARTIST,
        url::{UrlResource, url_source_key},
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
    pub artist: Option<RecordingRelationArtist>,
    pub url: Option<UrlResource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RecordingRelationArtist {
    pub id: String,
    pub name: String,
    #[serde(flatten)]
    pub extra: serde_json::Value,
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

pub(crate) const PARTS: &str = "artist-credits+artist-rels+url-rels";
/// No `+recordings`: MusicBrainz rejects it outright on this endpoint
/// ("recordings is not a valid inc parameter for the recording resource"),
/// unlike on a release lookup (`release.rs`'s own `PARTS`, where it *is*
/// valid) — this asymmetry is easy to miss since both look like ordinary
/// `inc` values. Without it, a track's medium never carries a `recording` id
/// back to disambiguate which of its tracks is *this* recording; see the
/// single-track-medium fallback in `get_recording` below.
pub(crate) const PARTS_WITH_RELEASES: &str = "artist-credits+artist-rels+url-rels+releases+media";

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

                let result = Ok(EntityResult {
                    release_date: None,
                    sources,
                    extra: r.extra.clone(),
                    specific_data: EntrySpecificData::Track {
                        duration_ms,
                        positions: Default::default(),
                    },
                    children: vec![Arc::new(artist_children)],
                    aliases,
                });
                async move { result }
            },
        )
        .await
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

        // Artist child: Haruka Karibu
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
        )
        .await?;

        assert!(matches!(
            rec.specific_data,
            EntrySpecificData::Track { ref duration_ms, .. } if duration_ms == &vec![208000_i64]
        ));

        Ok(())
    }
}
