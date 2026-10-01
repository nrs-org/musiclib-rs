use std::{collections::HashSet, future::Future, sync::Arc};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::providers::{
    backends::spotify::{
        SOURCE,
        canonicalize::{match_playlist_url, playlist_url, track_url},
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
pub(crate) struct PlaylistResponse {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub owner: PlaylistOwner,
    pub tracks: Paging<PlaylistItem>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistOwner {
    pub id: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistItem {
    pub track: Option<PlaylistTrack>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistTrack {
    pub id: String,
    pub name: String,
    pub duration_ms: i64,
    pub track_number: i32,
    pub disc_number: i32,
    pub artists: Vec<SimpleArtist>,
}

// --- Constants ---

pub(crate) const PLAYLIST_TRACKS_LIMIT: u32 = 100;

// --- Page fetcher ---

struct PlaylistItemsPageFetcher {
    client: SpotifyClient,
    playlist_id: String,
}

#[async_trait::async_trait]
impl PageFetcher for PlaylistItemsPageFetcher {
    async fn fetch_page(&mut self, page_token: Option<&str>) -> Result<ChildPage, Error> {
        let offset: u32 = page_token.and_then(|t| t.parse().ok()).unwrap_or(0);
        let limit_str = PLAYLIST_TRACKS_LIMIT.to_string();
        let offset_str = offset.to_string();

        let response = self
            .client
            .get::<Paging<PlaylistItem>, _, _, Error, _>(
                &format!("playlists/{}/tracks", self.playlist_id),
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

        let children = playlist_items_to_child_refs(&response.items);

        Ok(ChildPage {
            children,
            next_page_token,
        })
    }
}

fn playlist_items_to_child_refs(items: &[PlaylistItem]) -> Vec<ChildRef> {
    items
        .iter()
        .filter_map(|item| item.track.as_ref())
        .map(|track| {
            let contributions: Vec<Contribution> = track
                .artists
                .iter()
                .enumerate()
                .map(|(i, a)| Contribution {
                    role: StandardRoleNames::LISTED_ARTIST.into(),
                    main_artist: true,
                    source: SOURCE.into(),
                    extra: serde_json::json!({ "index": i, "artist_id": a.id, "artist_name": a.name }),
                })
                .collect();
            ChildRef {
                entry_type: EntryType::Track,
                external_type: EXTERNAL_TYPE_TRACK.into(),
                sources: [(SOURCE.into(), HashSet::from([track_url(&track.id)]))].into(),
                name: Some(track.name.clone()),
                duration_ms: Some(track.duration_ms),
                appears_on: false,
                position: Some(TrackPosition {
                    disc_no: if track.disc_number > 1 {
                        Some(track.disc_number)
                    } else {
                        None
                    },
                    track_no: track.track_number,
                    synthetic: false,
                }),
                contributions,
                original_relation_kind: None,
            }
        })
        .collect()
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

pub async fn get_playlist_raw<T, F, R, E, FR>(
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
    let id = match_playlist_url(url).expect("Invalid Spotify playlist URL");
    client
        .get::<T, _, _, _, _>(&format!("playlists/{id}"), &[], |value| callback(value, id))
        .await
}

pub async fn get_playlist(client: &SpotifyClient, url: &str) -> Result<EntityResult<()>, Error> {
    get_playlist_raw::<PlaylistResponse, _, _, Error, _>(client, url, |p, id| {
        let canonical = playlist_url(id);

        // Owner as an artist-typed child (uploader role).
        let owner_ref = ChildRef {
            entry_type: EntryType::Artist,
            external_type: EXTERNAL_TYPE_ARTIST.into(),
            // Spotify user profiles share the same open.spotify.com namespace but under /user/
            // — store just the id in the source set to avoid inventing a URL pattern.
            sources: [(
                SOURCE.into(),
                HashSet::from([format!("https://open.spotify.com/user/{}", p.owner.id)]),
            )]
            .into(),
            name: p.owner.display_name.clone(),
            contributions: vec![Contribution {
                role: StandardRoleNames::UPLOADER.into(),
                main_artist: false,
                source: SOURCE.into(),
                extra: serde_json::Value::Null,
            }],
            ..Default::default()
        };

        // Inline first page of tracks; paginate rest.
        let inline_count = p.tracks.items.len() as u32;
        let tracks_child: Arc<CachedChildSource<()>> = if inline_count >= p.tracks.total {
            Arc::new(CachedChildSource::from_children(
                playlist_items_to_child_refs(&p.tracks.items),
            ))
        } else {
            let paginator = PaginatedChildSource::new(Box::new(PlaylistItemsPageFetcher {
                client: client.clone(),
                playlist_id: id.to_string(),
            }))
            .with_static_eval(track_eval);
            Arc::new(CachedChildSource::new(Box::new(paginator)))
        };

        let result = Ok(EntityResult {
            release_date: None,
            sources: [(SOURCE.into(), HashSet::from([canonical]))].into(),
            extra: p.extra.clone(),
            specific_data: EntrySpecificData::Release {
                release_type: Some("playlist".into()),
                num_discs: None,
                num_tracks: Some(p.tracks.total as i32),
            },
            children: vec![
                Arc::new(CachedChildSource::from_children(vec![owner_ref])),
                tracks_child,
            ],
            aliases: vec![Alias {
                name: p.name.clone(),
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
                canonicalize::playlist_url,
                client::SpotifyClient,
                playlist::{PlaylistResponse, get_playlist},
                types::EXTERNAL_TYPE_TRACK,
            },
            types::{ChildSource, EntrySpecificData, EntryType},
        },
        test_utils::MockHttpClient,
    };

    fn playlist_api_url(id: &str) -> String {
        SpotifyClient::build_url(&format!("playlists/{id}"), &[])
    }

    #[tokio::test]
    async fn test_get_playlist() -> anyhow::Result<()> {
        let id = "5NYKuv10GX1WoQEODhgQIx";
        let mut http_client = MockHttpClient::new();
        http_client.add_route_json::<PlaylistResponse>(
            Method::GET,
            &playlist_api_url(id),
            include_str!("./playlist_charaalbum.json"),
        );

        let client = SpotifyClient::new_with_token(Arc::new(http_client), "test_token");
        let playlist = get_playlist(&client, &playlist_url(id)).await?;

        // Title and release type
        assert_eq!(playlist.aliases.len(), 1);
        assert_eq!(playlist.aliases[0].name, "charaalbum shortlist");
        assert!(playlist.aliases[0].primary);
        assert!(matches!(
            playlist.specific_data,
            EntrySpecificData::Release {
                release_type: Some(_),
                num_tracks: Some(5),
                ..
            }
        ));
        assert_eq!(
            playlist.sources.get(SOURCE).unwrap(),
            &std::collections::HashSet::from([format!("https://open.spotify.com/playlist/{id}")])
        );

        // children[0] = owner, children[1] = tracks
        assert_eq!(playlist.children.len(), 2);

        // Owner: lab slanderer
        let mut owner_cursor = playlist.children[0].cursor();
        let (owner, _) = child_next(&mut owner_cursor)
            .await?
            .expect("expected owner");
        assert_eq!(owner.entry_type, EntryType::Artist);
        assert_eq!(
            owner.sources.get(SOURCE).unwrap(),
            &std::collections::HashSet::from([
                "https://open.spotify.com/user/ngoduyanhtb".to_string()
            ])
        );

        // First track: ステラ
        let mut tracks = playlist.children[1].cursor();
        let (track, _) = child_next(&mut tracks)
            .await?
            .expect("expected first track");
        assert_eq!(track.entry_type, EntryType::Track);
        assert_eq!(track.external_type.as_ref(), EXTERNAL_TYPE_TRACK);
        assert_eq!(
            track.name.as_deref(),
            Some("ステラ (feat. 星乃一歌&天馬咲希&望月穂波&日野森志歩&初音ミク)")
        );
        assert_eq!(
            track.sources.get(SOURCE).unwrap(),
            &std::collections::HashSet::from([
                "https://open.spotify.com/track/22vyFWBbPhh9PcfrF15qPW".to_string()
            ])
        );

        Ok(())
    }
}
