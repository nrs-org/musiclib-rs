use std::{collections::HashSet, future::Future, sync::Arc};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::providers::{
    backends::musicbrainz::{
        SOURCE,
        artist_credit::{ArtistCredit, artist_credit_child_refs},
        canonicalize::{match_release_url, recording_url, release_url},
        client::MusicBrainzClient,
        types::EXTERNAL_TYPE_RECORDING,
        url::{UrlResource, url_source_key},
    },
    types::{
        Alias, CachedChildSource, ChildRef, EntityResult, EntrySpecificData, EntryType, Error,
        TrackPosition,
    },
};

// --- API response types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ReleaseResponse {
    pub id: String,
    pub title: String,
    pub date: Option<String>,
    pub status: Option<String>,
    pub country: Option<String>,
    pub disambiguation: Option<String>,
    #[serde(rename = "artist-credit", default)]
    pub artist_credit: Vec<ArtistCredit>,
    #[serde(default)]
    pub media: Vec<Medium>,
    #[serde(default)]
    pub relations: Vec<ReleaseRelation>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ReleaseRelation {
    #[serde(rename = "target-type")]
    pub target_type: String,
    pub url: Option<UrlResource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Medium {
    pub position: i32,
    #[serde(rename = "track-count", default)]
    pub track_count: i32,
    #[serde(default)]
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    pub position: i32,
    pub title: Option<String>,
    /// Track duration in milliseconds.
    pub length: Option<i64>,
    pub recording: Option<TrackRecording>,
    #[serde(rename = "artist-credit", default)]
    pub artist_credit: Vec<ArtistCredit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackRecording {
    pub id: String,
    pub title: Option<String>,
    pub length: Option<i64>,
    #[serde(rename = "artist-credit", default)]
    pub artist_credit: Vec<ArtistCredit>,
}

// --- Constants ---

pub(crate) const PARTS: &str = "recordings+artist-credits+url-rels";

// --- Helpers ---

fn parse_release_date(date_str: &str) -> Option<String> {
    // MB dates can be "YYYY", "YYYY-MM", or "YYYY-MM-DD".
    Some(match date_str.len() {
        4 => format!("{date_str}-XX-XX XX:XX:XX"),
        7 => format!("{date_str}-XX XX:XX:XX"),
        10 => format!("{date_str} XX:XX:XX"),
        _ => return None,
    })
}

// --- Public API ---

pub async fn get_release_raw<T, F, R, E, FR>(
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
    let mbid = match_release_url(url).expect("Invalid MusicBrainz release URL");
    client
        .get::<T, _, _, _, _>(&format!("release/{mbid}"), &[("inc", PARTS)], |value| {
            callback(value, mbid)
        })
        .await
}

pub async fn get_release(client: &MusicBrainzClient, url: &str) -> Result<EntityResult<()>, Error> {
    get_release_raw::<ReleaseResponse, _, _, Error, _>(client, url, |r, mbid| {
        let url = release_url(mbid);

        let release_date = r.date.as_deref().and_then(parse_release_date);

        let num_discs = r.media.len() as i32;
        let num_tracks: i32 = r.media.iter().map(|m| m.track_count).sum();

        // Flatten all tracks across all media into recording ChildRefs.
        let track_refs: Vec<ChildRef> = r
            .media
            .iter()
            .flat_map(|medium| {
                medium.tracks.iter().map(move |track| {
                    let rec = track.recording.as_ref();
                    let recording_mbid = rec.map(|r| r.id.as_str());
                    // Prefer track-level artist credit; fall back to recording's.
                    let credits = if !track.artist_credit.is_empty() {
                        &track.artist_credit
                    } else {
                        rec.map(|r| &r.artist_credit)
                            .unwrap_or(&track.artist_credit)
                    };
                    let contributions = artist_credit_child_refs(credits)
                        .into_iter()
                        .flat_map(|c| c.contributions)
                        .collect();
                    let sources = if let Some(id) = recording_mbid {
                        [(SOURCE.into(), HashSet::from([recording_url(id)]))].into()
                    } else {
                        Default::default()
                    };
                    ChildRef {
                        entry_type: EntryType::Track,
                        external_type: EXTERNAL_TYPE_RECORDING.into(),
                        sources,
                        name: track
                            .title
                            .clone()
                            .or_else(|| rec.and_then(|r| r.title.clone())),
                        duration_ms: track.length.or_else(|| rec.and_then(|r| r.length)),
                        appears_on: false,
                        position: Some(TrackPosition {
                            disc_no: if num_discs > 1 {
                                Some(medium.position)
                            } else {
                                None
                            },
                            track_no: track.position,
                            synthetic: false,
                        }),
                        contributions,
                        original_relation_kind: None,
                    }
                })
            })
            .collect();

        // Artist children from release-level artist credit.
        let artist_refs = artist_credit_child_refs(&r.artist_credit);

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

        let result = Ok(EntityResult {
            release_date,
            sources,
            extra: r.extra.clone(),
            specific_data: EntrySpecificData::Release {
                release_type: None,
                num_discs: Some(num_discs),
                num_tracks: Some(num_tracks),
            },
            children: vec![
                Arc::new(CachedChildSource::from_children(artist_refs)),
                Arc::new(CachedChildSource::from_children(track_refs)),
            ],
            aliases,
        });
        async move { result }
    })
    .await
}

#[cfg(test)]
mod tests {
    use crate::providers::types::child_next;
    use std::{collections::HashSet, sync::Arc};

    use http::Method;

    use crate::{
        providers::{
            backends::musicbrainz::{
                SOURCE,
                client::MusicBrainzClient,
                recording::{PARTS as RECORDING_PARTS, RecordingResponse, get_recording},
                release::{PARTS, ReleaseResponse, get_release},
                types::EXTERNAL_TYPE_RECORDING,
            },
            types::{ChildSource, EntrySpecificData, EntryType},
        },
        test_utils::MockHttpClient,
    };

    fn recording_api_url(mbid: &str) -> String {
        MusicBrainzClient::build_url(&format!("recording/{mbid}"), &[("inc", RECORDING_PARTS)])
    }

    fn release_api_url(mbid: &str) -> String {
        MusicBrainzClient::build_url(&format!("release/{mbid}"), &[("inc", PARTS)])
    }

    #[tokio::test]
    async fn test_get_release() -> anyhow::Result<()> {
        let mbid = "87865438-2b9b-4462-bb9e-73ea1873108e";
        let mut http_client = MockHttpClient::new();
        http_client.add_route_json::<ReleaseResponse>(
            Method::GET,
            &release_api_url(mbid),
            include_str!("./release_watame_lullaby.json"),
        );

        let client = MusicBrainzClient::new_with_client(Arc::new(http_client), None)?;
        let rel = get_release(&client, &format!("https://musicbrainz.org/release/{mbid}")).await?;

        // Date
        assert_eq!(rel.release_date.as_deref(), Some("2021-03-07 XX:XX:XX"));

        // Sources
        assert_eq!(
            rel.sources.get(SOURCE).unwrap(),
            &HashSet::from([format!("https://musicbrainz.org/release/{mbid}")])
        );

        // Specific data
        assert!(matches!(
            rel.specific_data,
            EntrySpecificData::Release {
                num_discs: Some(1),
                num_tracks: Some(1),
                ..
            }
        ));

        // Alias
        assert_eq!(rel.aliases.len(), 1);
        assert_eq!(rel.aliases[0].name, "Watame Lullaby");

        // children[0] = artist credits, children[1] = tracks
        assert_eq!(rel.children.len(), 2);

        // Track child
        let mut tracks_cursor = rel.children[1].cursor();
        let (track, _) = child_next(&mut tracks_cursor)
            .await?
            .expect("expected track");
        assert_eq!(track.entry_type, EntryType::Track);
        assert_eq!(track.external_type.as_ref(), EXTERNAL_TYPE_RECORDING);
        assert_eq!(
            track.sources.get(SOURCE).unwrap(),
            &HashSet::from([
                "https://musicbrainz.org/recording/1c75e623-32dc-4e2d-82d2-72d966cbe82b"
                    .to_string()
            ])
        );
        assert_eq!(track.name.as_deref(), Some("Watame Lullaby"));
        let pos = track.position.as_ref().expect("expected position");
        assert_eq!(pos.track_no, 1);
        assert_eq!(pos.disc_no, None); // single-disc: disc_no is None

        Ok(())
    }

    #[tokio::test]
    async fn test_get_release_hop_step_sheep() -> anyhow::Result<()> {
        let mbid = "c9496f63-4978-4008-b069-953e8fb1f8a6";
        let recording_mbid = "d70b0d40-1dac-4703-a025-39073ead3e08";
        let mut http_client = MockHttpClient::new();
        http_client.add_route_json::<ReleaseResponse>(
            Method::GET,
            &release_api_url(mbid),
            include_str!("./release_watame_hop_step_sheep.json"),
        );
        http_client.add_route_json::<RecordingResponse>(
            Method::GET,
            &recording_api_url(recording_mbid),
            include_str!("./recording_beautiful_circle.json"),
        );

        let client = MusicBrainzClient::new_with_client(Arc::new(http_client), None)?;
        let rel = get_release(&client, &format!("https://musicbrainz.org/release/{mbid}")).await?;

        // Date
        assert_eq!(rel.release_date.as_deref(), Some("2024-01-10 XX:XX:XX"));

        // Sources
        assert_eq!(
            rel.sources.get(SOURCE).unwrap(),
            &HashSet::from([format!("https://musicbrainz.org/release/{mbid}")])
        );

        // Specific data: 1 disc, 10 tracks
        assert!(matches!(
            rel.specific_data,
            EntrySpecificData::Release {
                num_discs: Some(1),
                num_tracks: Some(10),
                ..
            }
        ));

        // Alias
        assert_eq!(rel.aliases.len(), 1);
        assert_eq!(rel.aliases[0].name, "Hop Step Sheep");

        // children[0] = artist credits, children[1] = tracks
        assert_eq!(rel.children.len(), 2);

        // First track
        let mut tracks_cursor = rel.children[1].cursor();
        let (track, _) = child_next(&mut tracks_cursor)
            .await?
            .expect("expected first track");
        assert_eq!(track.entry_type, EntryType::Track);
        assert_eq!(track.external_type.as_ref(), EXTERNAL_TYPE_RECORDING);
        assert_eq!(track.name.as_deref(), Some("Beautiful Circle"));
        let pos = track.position.as_ref().expect("expected position");
        assert_eq!(pos.track_no, 1);
        assert_eq!(pos.disc_no, None); // single-disc

        // Fetch recording metadata for the first track
        let recording_url = track
            .sources
            .get(SOURCE)
            .and_then(|s| s.iter().next())
            .expect("expected recording source url")
            .clone();
        let rec = get_recording(&client, &recording_url, false, true).await?;

        assert_eq!(rec.aliases.len(), 1);
        assert_eq!(rec.aliases[0].name, "Beautiful Circle");
        assert!(matches!(
            rec.specific_data,
            EntrySpecificData::Track {
                ref duration_ms,
                ..
            } if duration_ms == &vec![229000_i64]
        ));
        assert_eq!(
            rec.sources.get(SOURCE).unwrap(),
            &HashSet::from([format!(
                "https://musicbrainz.org/recording/{recording_mbid}"
            )])
        );

        // 1 child source containing: 1 artist-credit (Watame) + 15 artist-rels
        assert_eq!(rec.children.len(), 1);
        let mut rec_artists = rec.children[0].cursor();
        let (main_artist, _) = child_next(&mut rec_artists)
            .await?
            .expect("expected main artist");
        assert_eq!(main_artist.name.as_deref(), Some("角巻わため"));
        assert!(main_artist.contributions[0].main_artist);

        // First relation: arranger 佐藤純一
        let (arranger, _) = child_next(&mut rec_artists)
            .await?
            .expect("expected arranger");
        assert_eq!(arranger.name.as_deref(), Some("佐藤純一"));
        assert_eq!(arranger.contributions[0].role, "arranger");
        assert!(!arranger.contributions[0].main_artist);

        Ok(())
    }
}
