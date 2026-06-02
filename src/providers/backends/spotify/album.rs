use std::{collections::HashSet, future::Future, sync::Arc};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::providers::{
    backends::spotify::{
        SOURCE,
        canonicalize::{album_url, artist_url, match_album_url, track_url},
        client::SpotifyClient,
        types::{EXTERNAL_TYPE_ARTIST, EXTERNAL_TYPE_TRACK, Paging, SimpleArtist},
    },
    std_values::StandardRoleNames,
    types::{
        Alias, CachedChildSource, ChildPage, ChildRef, CompiledChildMatcher,
        CompiledEntryDataMatcher, CompiledMatcherExpr, Contribution, EntityResult,
        EntrySpecificData, EntryType, Error, PageFetcher, PaginatedChildSource, TrackPosition,
        Tribool, default_eval_leaf, static_eval_expr,
    },
};

// --- API response types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AlbumResponse {
    pub id: String,
    pub name: String,
    pub album_type: Option<String>,
    pub release_date: Option<String>,
    pub release_date_precision: Option<String>,
    pub total_tracks: u32,
    pub artists: Vec<SimpleArtist>,
    pub tracks: Paging<SimplifiedTrack>,
    #[serde(default)]
    pub external_ids: AlbumExternalIds,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimplifiedTrack {
    pub id: String,
    pub name: String,
    pub duration_ms: i64,
    pub track_number: i32,
    pub disc_number: i32,
    pub artists: Vec<SimpleArtist>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AlbumExternalIds {
    pub upc: Option<String>,
    pub isrc: Option<String>,
    pub ean: Option<String>,
}

// --- Helpers ---

pub(crate) fn parse_release_date(date_str: &str, precision: Option<&str>) -> Option<String> {
    Some(match precision {
        Some("year") | None if date_str.len() == 4 => format!("{date_str}-XX-XX XX:XX:XX"),
        Some("month") | None if date_str.len() == 7 => format!("{date_str}-XX XX:XX:XX"),
        _ if date_str.len() == 10 => format!("{date_str} XX:XX:XX"),
        _ => return None,
    })
}

fn simplified_track_to_child_ref(track: &SimplifiedTrack) -> ChildRef {
    ChildRef {
        entry_type: EntryType::Track,
        external_type: EXTERNAL_TYPE_TRACK.into(),
        sources: [(SOURCE.into(), HashSet::from([track_url(&track.id)]))].into(),
        name: Some(track.name.clone()),
        position: Some(TrackPosition {
            disc_no: if track.disc_number > 1 {
                Some(track.disc_number)
            } else {
                None
            },
            track_no: track.track_number,
        }),
        contributions: vec![],
    }
}

pub(crate) const ALBUM_TRACKS_LIMIT: u32 = 50;

// --- Page fetcher ---

struct AlbumTracksPageFetcher {
    client: SpotifyClient,
    album_id: String,
}

#[async_trait::async_trait]
impl PageFetcher for AlbumTracksPageFetcher {
    async fn fetch_page(&mut self, page_token: Option<&str>) -> Result<ChildPage, Error> {
        let offset: u32 = page_token.and_then(|t| t.parse().ok()).unwrap_or(0);
        let limit_str = ALBUM_TRACKS_LIMIT.to_string();
        let offset_str = offset.to_string();

        let response = self
            .client
            .get::<Paging<SimplifiedTrack>, _, _, Error, _>(
                &format!("albums/{}/tracks", self.album_id),
                &[
                    ("limit", limit_str.as_str()),
                    ("offset", offset_str.as_str()),
                ],
                |r| {
                    let r = r.clone();
                    async move { Ok(r) }
                },
            )
            .await?;

        let fetched = response.items.len() as u32;
        let next_offset = offset + fetched;
        let next_page_token = if next_offset < response.total {
            Some(next_offset.to_string())
        } else {
            None
        };

        let children = response
            .items
            .iter()
            .map(simplified_track_to_child_ref)
            .collect();

        Ok(ChildPage {
            children,
            next_page_token,
        })
    }
}

// --- Static eval helpers ---

fn track_eval(expr: &CompiledMatcherExpr) -> Tribool {
    static_eval_expr(expr, &|matcher| match matcher {
        CompiledChildMatcher::EntryData(CompiledEntryDataMatcher::EntryType(t)) => {
            (*t == EntryType::Track).into()
        }
        CompiledChildMatcher::EntryData(CompiledEntryDataMatcher::ExternalType(t)) => {
            (t == EXTERNAL_TYPE_TRACK).into()
        }
        _ => default_eval_leaf(matcher),
    })
}

// --- Public API ---

pub async fn get_album_raw<T, F, R, E, FR>(
    client: &SpotifyClient,
    url: &str,
    callback: F,
) -> Result<R, E>
where
    T: std::any::Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
    F: FnOnce(&T, &str) -> FR,
    E: From<Error> + Send + 'static,
    FR: Future<Output = Result<R, E>> + Send + 'static,
{
    let id = match_album_url(url).expect("Invalid Spotify album URL");
    client
        .get::<T, _, _, _, _>(&format!("albums/{id}"), &[], |value| callback(value, id))
        .await
}

pub async fn get_album(client: &SpotifyClient, url: &str) -> Result<EntityResult<()>, Error> {
    get_album_raw::<AlbumResponse, _, _, Error, _>(client, url, |a, id| {
        let canonical = album_url(id);

        let release_date = a
            .release_date
            .as_deref()
            .and_then(|d| parse_release_date(d, a.release_date_precision.as_deref()));

        let artist_refs: Vec<ChildRef> = a
            .artists
            .iter()
            .enumerate()
            .map(|(i, art)| ChildRef {
                entry_type: EntryType::Artist,
                external_type: EXTERNAL_TYPE_ARTIST.into(),
                sources: [(SOURCE.into(), HashSet::from([artist_url(&art.id)]))].into(),
                name: Some(art.name.clone()),
                contributions: vec![Contribution {
                    role: StandardRoleNames::LISTED_ARTIST.into(),
                    main_artist: true,
                    source: SOURCE.into(),
                    extra: serde_json::json!({ "index": i }),
                }],
                ..Default::default()
            })
            .collect();

        // Use inline tracks if they cover the full album; otherwise paginate from offset 0.
        let inline_count = a.tracks.items.len() as u32;
        let tracks_child: Arc<CachedChildSource<()>> = if inline_count >= a.tracks.total {
            let track_refs: Vec<ChildRef> = a
                .tracks
                .items
                .iter()
                .map(simplified_track_to_child_ref)
                .collect();
            Arc::new(CachedChildSource::from_children(track_refs))
        } else {
            let paginator = PaginatedChildSource::new(Box::new(AlbumTracksPageFetcher {
                client: client.clone(),
                album_id: id.to_string(),
            }))
            .with_static_eval(track_eval);
            Arc::new(CachedChildSource::new(Box::new(paginator)))
        };

        let mut sources: crate::providers::types::ExternalSources =
            [(SOURCE.into(), HashSet::from([canonical]))].into();
        if let Some(upc) = &a.external_ids.upc {
            sources
                .0
                .entry("upc".into())
                .or_default()
                .insert(upc.clone());
        }

        let result = Ok(EntityResult {
            release_date,
            sources,
            extra: a.extra.clone(),
            specific_data: EntrySpecificData::Release {
                release_type: a.album_type.clone(),
                num_discs: None,
                num_tracks: Some(a.total_tracks as i32),
            },
            children: vec![
                Arc::new(CachedChildSource::from_children(artist_refs)),
                tracks_child,
            ],
            aliases: vec![Alias {
                name: a.name.clone(),
                source: SOURCE.into(),
                primary: true,
                ..Default::default()
            }],
        });
        async move { result }
    })
    .await
}

#[cfg(test)]
mod tests {
    use crate::providers::types::child_next;
    use std::sync::Arc;

    use http::Method;

    use crate::{
        providers::{
            backends::spotify::{
                SOURCE,
                album::{AlbumResponse, get_album},
                canonicalize::album_url,
                client::SpotifyClient,
                types::{EXTERNAL_TYPE_ARTIST, EXTERNAL_TYPE_TRACK},
            },
            types::{ChildSource, EntrySpecificData, EntryType},
        },
        test_utils::MockHttpClient,
    };

    fn album_api_url(id: &str) -> String {
        SpotifyClient::build_url(&format!("albums/{id}"), &[])
    }

    #[tokio::test]
    async fn test_get_album() -> anyhow::Result<()> {
        let id = "0LCmjkgN0CqcPtLuNHUBma";
        let mut http_client = MockHttpClient::new();
        http_client.add_route_json::<AlbumResponse>(
            Method::GET,
            &album_api_url(id),
            include_str!("./album_hop_step_sheep.json"),
        );

        let client = SpotifyClient::new_with_token(Arc::new(http_client), "test_token");
        let album = get_album(&client, &album_url(id)).await?;

        // Title and release metadata
        assert_eq!(album.aliases.len(), 1);
        assert_eq!(album.aliases[0].name, "Hop Step Sheep");
        assert!(album.aliases[0].primary);
        assert!(matches!(
            album.specific_data,
            EntrySpecificData::Release {
                release_type: Some(_),
                num_tracks: Some(10),
                ..
            }
        ));

        // Release date: 2024-01-10
        assert_eq!(album.release_date.as_deref(), Some("2024-01-10 XX:XX:XX"));

        // Sources
        assert_eq!(
            album.sources.get(SOURCE).unwrap(),
            &std::collections::HashSet::from([format!("https://open.spotify.com/album/{id}")])
        );

        // children[0] = artists, children[1] = tracks
        assert_eq!(album.children.len(), 2);

        let mut artists = album.children[0].cursor();
        let (artist, _) = child_next(&mut artists).await?.expect("expected artist");
        assert_eq!(artist.entry_type, EntryType::Artist);
        assert_eq!(artist.external_type.as_ref(), EXTERNAL_TYPE_ARTIST);
        assert_eq!(artist.name.as_deref(), Some("角巻わため"));
        assert_eq!(
            artist.sources.get(SOURCE).unwrap(),
            &std::collections::HashSet::from([
                "https://open.spotify.com/artist/68609MOnEU86kVyMf26JnM".to_string()
            ])
        );

        // All 10 tracks are inline — first track is Beautiful Circle
        let mut tracks = album.children[1].cursor();
        let (track, _) = child_next(&mut tracks)
            .await?
            .expect("expected first track");
        assert_eq!(track.entry_type, EntryType::Track);
        assert_eq!(track.external_type.as_ref(), EXTERNAL_TYPE_TRACK);
        assert_eq!(track.name.as_deref(), Some("Beautiful Circle"));
        assert_eq!(
            track.sources.get(SOURCE).unwrap(),
            &std::collections::HashSet::from([
                "https://open.spotify.com/track/4C3klfwXqCFv61VJ4bP7oJ".to_string()
            ])
        );
        let pos = track.position.as_ref().expect("expected position");
        assert_eq!(pos.track_no, 1);
        assert_eq!(pos.disc_no, None);

        Ok(())
    }
}
