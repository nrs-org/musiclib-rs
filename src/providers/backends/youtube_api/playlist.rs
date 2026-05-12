use std::collections::HashSet;

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
        },
    },
    types::{Alias, ChildRef, EntityResult, EntrySpecificData, EntryType, Error},
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

async fn fetch_playlist_items(
    client: &YoutubeClient,
    playlist_id: &str,
) -> Result<Vec<ChildRef>, Error> {
    let mut children = Vec::new();
    let mut page_token: Option<String> = None;

    loop {
        let mut params = vec![
            ("part", PLAYLIST_ITEM_PARTS.to_string()),
            ("playlistId", playlist_id.to_string()),
            ("maxResults", PLAYLIST_ITEMS_MAX_RESULTS.to_string()),
        ];
        if let Some(token) = &page_token {
            params.push(("pageToken", token.clone()));
        }
        let params_ref: Vec<(&str, &str)> = params
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .collect();
        let (items, next_page_token) = client
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

        for item in items {
            let Some(video_id) = item.snippet.resource_id.video_id else {
                continue;
            };
            children.push(ChildRef {
                entry_type: EntryType::Track,
                sources: [(SOURCE.into(), HashSet::from([video_url(&video_id)]))].into(),
                name: item.snippet.title.clone(),
                ..Default::default()
            });
        }

        if next_page_token.is_none() {
            break;
        }
        page_token = next_page_token;
    }

    Ok(children)
}

pub async fn get_playlist(client: &YoutubeClient, url: &str) -> Result<EntityResult, Error> {
    let id = match_playlist_url(url).expect("Invalid YouTube playlist URL");
    let url = playlist_url(id);
    let children = fetch_playlist_items(client, id).await?;
    let result = get_playlist_raw::<PlaylistListResponse, _, _, Error, _>(
        client,
        url.as_str(),
        move |p, id| {
            let p = &p.items[0];
            let url = playlist_url(id);
            let release_date = OffsetDateTime::parse(&p.snippet.published_at, &Rfc3339).ok();
            let result = Ok(EntityResult {
                release_date,
                sources: [(SOURCE.into(), HashSet::from([url.to_string()]))].into(),
                extra: serde_json::to_value(p).unwrap_or_default(),
                specific_data: EntrySpecificData::Release {
                    release_type: Some("playlist".into()),
                    num_discs: None,
                    num_tracks: None,
                },
                children,
                aliases: vec![Alias {
                    name: p.snippet.title.clone(),
                    source: SOURCE.into(),
                    primary: true,
                    ..Default::default()
                }],
            });
            async move { result }
        },
    )
    .await?;
    Ok(result)
}

#[cfg(test)]
mod tests {
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
            types::EntrySpecificData,
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

        assert!(playlist.release_date.is_some());
        assert_eq!(playlist.release_date.unwrap().year(), 2021);
        assert_eq!(playlist.release_date.unwrap().month(), time::Month::June);
        assert_eq!(playlist.release_date.unwrap().day(), 27);
        assert_eq!(playlist.sources.len(), 1);
        assert_eq!(
            playlist.sources.get(SOURCE).unwrap(),
            &HashSet::from([format!("https://www.youtube.com/playlist?list={id}")])
        );
        assert!(matches!(
            playlist.specific_data,
            EntrySpecificData::Release { .. }
        ));
        assert_eq!(playlist.children.len(), 32);
        let child = &playlist.children[0];
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
