use std::{collections::HashSet, sync::Arc};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::future::Future;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::providers::{
    backends::{
        ExtraJSON,
        youtube_api::{
            SOURCE,
            canonicalize::{match_playlist_url, playlist_url, video_url},
            client::YoutubeClient,
            types::EXTERNAL_TYPE_VIDEO,
        },
    },
    types::{
        Alias, CachedChildSource, ChildPage, ChildRef, CompiledChildMatcher,
        CompiledEntryDataMatcher, EntityResult, EntrySpecificData, EntryType, Error, PageFetcher,
        PaginatedChildSource, default_eval_leaf, static_eval_expr,
    },
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlaylistListResponse {
    items: Vec<Playlist>,
    #[serde(flatten)]
    extra: ExtraJSON,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Playlist {
    snippet: PlaylistSnippet,
    #[serde(flatten)]
    extra: ExtraJSON,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlaylistSnippet {
    title: String,
    #[serde(rename = "publishedAt")]
    published_at: String,
    #[serde(flatten)]
    extra: ExtraJSON,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlaylistItemsResponse {
    items: Vec<PlaylistItem>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
    #[serde(flatten)]
    extra: ExtraJSON,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlaylistItem {
    snippet: PlaylistItemSnippet,
    #[serde(flatten)]
    extra: ExtraJSON,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlaylistItemSnippet {
    title: Option<String>,
    #[serde(rename = "resourceId")]
    resource_id: PlaylistItemResourceId,
    #[serde(flatten)]
    extra: ExtraJSON,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlaylistItemResourceId {
    #[serde(rename = "videoId")]
    video_id: Option<String>,
    #[serde(flatten)]
    extra: ExtraJSON,
}

const PARTS: &str = "snippet";
const PLAYLIST_ITEM_PARTS: &str = "snippet";
const PLAYLIST_ITEMS_MAX_RESULTS: &str = "50";

pub async fn get_playlist_raw<T, F, R, E, FR>(
    client: &YoutubeClient,
    url: &str,
    callback: F,
) -> Result<R, E>
where
    T: std::any::Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
    F: FnOnce(&T, &str) -> FR,
    E: From<Error> + Send + 'static,
    FR: Future<Output = Result<R, E>> + Send + 'static,
{
    let id = match_playlist_url(url).expect("Invalid YouTube playlist URL");
    client
        .get::<T, _, _, _, _>("playlists", &[("part", PARTS), ("id", id)], |value| {
            callback(value, id)
        })
        .await
}

pub async fn get_playlist_items_raw<T, F, R, E, FR>(
    client: &YoutubeClient,
    url: &str,
    callback: F,
) -> Result<R, E>
where
    T: std::any::Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
    F: FnOnce(&T, &str) -> FR,
    E: From<Error> + Send + 'static,
    FR: Future<Output = Result<R, E>> + Send + 'static,
{
    let id = match_playlist_url(url).expect("Invalid YouTube playlist URL");
    client
        .get::<T, _, _, _, _>(
            "playlistItems",
            &[
                ("part", PLAYLIST_ITEM_PARTS),
                ("playlistId", id),
                ("maxResults", PLAYLIST_ITEMS_MAX_RESULTS),
            ],
            |value| callback(value, id),
        )
        .await
}

struct PlaylistItemsPageFetcher {
    client: YoutubeClient,
    playlist_id: String,
}

#[async_trait::async_trait]
impl PageFetcher for PlaylistItemsPageFetcher {
    async fn fetch_page(&mut self, page_token: Option<&str>) -> Result<ChildPage, Error> {
        let mut params = vec![
            ("part", PLAYLIST_ITEM_PARTS.to_string()),
            ("playlistId", self.playlist_id.clone()),
            ("maxResults", PLAYLIST_ITEMS_MAX_RESULTS.to_string()),
        ];
        if let Some(token) = page_token {
            params.push(("pageToken", token.to_string()));
        }
        let params_ref: Vec<(&str, &str)> = params
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .collect();
        let (items, next_page_token) = self
            .client
            .get::<PlaylistItemsResponse, _, _, Error, _>(
                "playlistItems",
                &params_ref,
                move |response| {
                    let items = response.items.clone();
                    let next_page_token = response.next_page_token.clone();
                    async move { Ok((items, next_page_token)) }
                },
            )
            .await?;

        let children = items
            .into_iter()
            .filter_map(|item| {
                let video_id = item.snippet.resource_id.video_id?;
                Some(ChildRef {
                    entry_type: EntryType::Track,
                    sources: [(SOURCE.into(), HashSet::from([video_url(&video_id)]))].into(),
                    name: item.snippet.title.clone(),
                    external_type: EXTERNAL_TYPE_VIDEO.into(),
                    ..Default::default()
                })
            })
            .collect();

        Ok(ChildPage {
            children,
            next_page_token,
        })
    }
}

pub async fn get_playlist(client: &YoutubeClient, url: &str) -> Result<EntityResult<()>, Error> {
    let id = match_playlist_url(url).expect("Invalid YouTube playlist URL");
    let url = playlist_url(id);
    let children_source = PaginatedChildSource::new(Box::new(PlaylistItemsPageFetcher {
        client: client.clone(),
        playlist_id: id.to_string(),
    }))
    .with_static_eval(|expr| {
        static_eval_expr(expr, &|matcher| match matcher {
            CompiledChildMatcher::EntryData(CompiledEntryDataMatcher::EntryType(t)) => {
                (*t == EntryType::Track).into()
            }
            _ => default_eval_leaf(matcher),
        })
    });
    let result = get_playlist_raw::<PlaylistListResponse, _, _, Error, _>(
        client,
        url.as_str(),
        move |p, id| {
            let p = p.items.first().cloned();
            let id = id.to_string();
            async move {
                let id = id.as_str();
                let p =
                    p.ok_or_else(|| Error::NotFound(format!("YouTube playlist not found: {id}")))?;
                let url = playlist_url(id);
                let release_date = OffsetDateTime::parse(&p.snippet.published_at, &Rfc3339)
                    .ok()
                    .map(|dt| {
                        format!(
                            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                            dt.year(),
                            dt.month() as u8,
                            dt.day(),
                            dt.hour(),
                            dt.minute(),
                            dt.second(),
                        )
                    });
                Ok(EntityResult {
                    release_date,
                    sources: [(SOURCE.into(), HashSet::from([url.to_string()]))].into(),
                    extra: serde_json::to_value(&p).unwrap_or_default(),
                    specific_data: EntrySpecificData::Release {
                        release_type: Some("playlist".into()),
                        num_discs: None,
                        num_tracks: None,
                    },
                    children: vec![Arc::new(CachedChildSource::new(Box::new(children_source)))],
                    aliases: vec![Alias {
                        name: p.snippet.title.clone(),
                        source: SOURCE.into(),
                        primary: true,
                        ..Default::default()
                    }],
                })
            }
        },
    )
    .await?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use crate::providers::types::child_next;
    use std::{collections::HashSet, sync::Arc};

    use http::Method;

    use crate::{
        providers::{
            backends::youtube_api::{
                SOURCE,
                client::YoutubeClient,
                playlist::{
                    PARTS, PLAYLIST_ITEM_PARTS, PLAYLIST_ITEMS_MAX_RESULTS, PlaylistItemsResponse,
                    PlaylistListResponse, get_playlist,
                },
            },
            types::{ChildSource, EntrySpecificData},
        },
        test_utils::MockHttpClient,
    };

    fn playlist_api_url(id: &str) -> String {
        YoutubeClient::build_url("playlists", &[("part", PARTS), ("id", id)])
    }

    fn playlist_items_api_url(id: &str) -> String {
        YoutubeClient::build_url(
            "playlistItems",
            &[
                ("part", PLAYLIST_ITEM_PARTS),
                ("playlistId", id),
                ("maxResults", PLAYLIST_ITEMS_MAX_RESULTS),
            ],
        )
    }

    #[tokio::test]
    async fn test_get_playlist_with_children() -> anyhow::Result<()> {
        let id = "PLZ34fLWik_iBK39rTWRAs_G93pnUW3K-7";
        let api_key = "doesnotmatter";

        let mut http_client = MockHttpClient::new();
        http_client.add_route_json::<PlaylistListResponse>(
            Method::GET,
            &playlist_api_url(id),
            include_str!("./playlist_watame_original_song.json"),
        );
        http_client.add_route_json::<PlaylistItemsResponse>(
            Method::GET,
            &playlist_items_api_url(id),
            include_str!("./playlist_watame_original_song_items_page1.json"),
        );

        let client = YoutubeClient::new_with_client(Arc::new(http_client), api_key.to_string())?;

        let playlist = get_playlist(
            &client,
            &format!("https://www.youtube.com/playlist?list={id}"),
        )
        .await?;

        assert_eq!(
            playlist.release_date.as_deref(),
            Some("2021-06-27 23:12:09")
        );
        assert_eq!(playlist.sources.len(), 1);
        assert_eq!(
            playlist.sources.get(SOURCE).unwrap(),
            &HashSet::from([format!("https://www.youtube.com/playlist?list={id}")])
        );
        assert!(matches!(
            playlist.specific_data,
            EntrySpecificData::Release { .. }
        ));
        let mut children_cursor = playlist.children[0].cursor();
        let mut children_vec = Vec::new();
        while let Some((child, _)) = child_next(&mut children_cursor).await? {
            children_vec.push(child);
        }
        assert_eq!(children_vec.len(), 32);
        let child = &children_vec[0];
        assert_eq!(child.entry_type, crate::providers::types::EntryType::Track);
        assert_eq!(
            child.sources.get(SOURCE).unwrap(),
            &HashSet::from(["https://youtu.be/2wClKY6mmyU".to_string()])
        );
        assert_eq!(
            child.name.as_ref().unwrap(),
            "ぼくらのなぞなぞ／角巻わため【original】"
        );

        Ok(())
    }
}
