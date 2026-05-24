use std::{
    collections::HashSet,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures::{Stream, StreamExt, stream::BoxStream};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::future::Future;

use crate::{
    http::{HttpClient, Request},
    providers::{
        backends::{
            ExtraJSON,
            youtube_api::{
                SOURCE,
                canonicalize::{
                    ChannelKind, channel_url, match_channel_url, playlist_url, video_url,
                },
                client::YoutubeClient,
                types::{EXTERNAL_TYPE_PLAYLIST, EXTERNAL_TYPE_VIDEO},
            },
        },
        types::{
            Alias, CachedChildSource, ChildPage, ChildRef, ChildSource, CompiledChildMatcher,
            CompiledEntryDataMatcher, CompiledMatcherExpr, EntityResult, EntrySpecificData,
            EntryType, Error, PageFetcher, PaginatedChildSource, Tribool, default_eval_leaf,
            static_eval_expr,
        },
    },
};

// --- API response types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChannelListResponse {
    items: Vec<Channel>,
    #[serde(flatten)]
    extra: ExtraJSON,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Channel {
    snippet: ChannelSnippet,
    #[serde(rename = "contentDetails")]
    content_details: ChannelContentDetails,
    #[serde(flatten)]
    extra: ExtraJSON,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChannelSnippet {
    title: String,
    #[serde(rename = "customUrl")]
    custom_url: Option<String>,
    #[serde(flatten)]
    extra: ExtraJSON,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChannelContentDetails {
    #[serde(rename = "relatedPlaylists")]
    related_playlists: RelatedPlaylists,
    #[serde(flatten)]
    extra: ExtraJSON,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RelatedPlaylists {
    uploads: String,
    #[serde(flatten)]
    extra: ExtraJSON,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChannelPlaylistsResponse {
    items: Vec<ChannelPlaylist>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
    #[serde(flatten)]
    extra: ExtraJSON,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChannelPlaylist {
    id: String,
    snippet: ChannelPlaylistSnippet,
    #[serde(flatten)]
    extra: ExtraJSON,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChannelPlaylistSnippet {
    title: Option<String>,
    #[serde(flatten)]
    extra: ExtraJSON,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UploadsPlaylistItemsResponse {
    items: Vec<UploadsPlaylistItem>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
    #[serde(flatten)]
    extra: ExtraJSON,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UploadsPlaylistItem {
    snippet: UploadsPlaylistItemSnippet,
    #[serde(flatten)]
    extra: ExtraJSON,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UploadsPlaylistItemSnippet {
    title: Option<String>,
    #[serde(rename = "resourceId")]
    resource_id: UploadsPlaylistItemResourceId,
    #[serde(flatten)]
    extra: ExtraJSON,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UploadsPlaylistItemResourceId {
    #[serde(rename = "videoId")]
    video_id: Option<String>,
    #[serde(flatten)]
    extra: ExtraJSON,
}

// --- ytmusicapi response types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
struct YtmusicRelease {
    title: Option<String>,
    #[serde(rename = "audioPlaylistId")]
    audio_playlist_id: Option<String>,
    #[serde(rename = "browseId")]
    browse_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct YtmusicAlbumResponse {
    #[serde(rename = "audioPlaylistId")]
    audio_playlist_id: Option<String>,
}

// --- Constants ---

pub(crate) const CHANNEL_PARTS: &str = "snippet,contentDetails";
pub(crate) const PLAYLISTS_PARTS: &str = "snippet";
pub(crate) const PLAYLISTS_MAX_RESULTS: &str = "50";
pub(crate) const UPLOADS_PARTS: &str = "snippet";
pub(crate) const UPLOADS_MAX_RESULTS: &str = "50";

// --- Page fetchers ---

/// Fetches pages of playlists owned by a channel (→ `EntryType::Release`).
struct ChannelPlaylistsPageFetcher {
    client: YoutubeClient,
    channel_id: String,
}

#[async_trait::async_trait]
impl PageFetcher for ChannelPlaylistsPageFetcher {
    async fn fetch_page(&mut self, page_token: Option<&str>) -> Result<ChildPage, Error> {
        let mut params = vec![
            ("part", PLAYLISTS_PARTS.to_string()),
            ("channelId", self.channel_id.clone()),
            ("maxResults", PLAYLISTS_MAX_RESULTS.to_string()),
        ];
        if let Some(token) = page_token {
            params.push(("pageToken", token.to_string()));
        }
        let params_ref: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();

        let (items, next_page_token) = self
            .client
            .get::<ChannelPlaylistsResponse, _, _, Error, _>("playlists", &params_ref, |response| {
                let items = response.items.clone();
                let next_page_token = response.next_page_token.clone();
                async move { Ok((items, next_page_token)) }
            })
            .await?;

        let children = items
            .into_iter()
            .map(|p| ChildRef {
                entry_type: EntryType::Release,
                sources: [(SOURCE.into(), HashSet::from([playlist_url(&p.id)]))].into(),
                name: p.snippet.title,
                external_type: EXTERNAL_TYPE_PLAYLIST.into(),
                ..Default::default()
            })
            .collect();

        Ok(ChildPage {
            children,
            next_page_token,
        })
    }
}

/// Fetches pages of videos from the channel's uploads playlist (→ `EntryType::Track`).
struct ChannelUploadsPageFetcher {
    client: YoutubeClient,
    uploads_playlist_id: String,
}

#[async_trait::async_trait]
impl PageFetcher for ChannelUploadsPageFetcher {
    async fn fetch_page(&mut self, page_token: Option<&str>) -> Result<ChildPage, Error> {
        let mut params = vec![
            ("part", UPLOADS_PARTS.to_string()),
            ("playlistId", self.uploads_playlist_id.clone()),
            ("maxResults", UPLOADS_MAX_RESULTS.to_string()),
        ];
        if let Some(token) = page_token {
            params.push(("pageToken", token.to_string()));
        }
        let params_ref: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();

        let (items, next_page_token) = self
            .client
            .get::<UploadsPlaylistItemsResponse, _, _, Error, _>(
                "playlistItems",
                &params_ref,
                |response| {
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
                    name: item.snippet.title,
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

/// Fetches the full YTMusic discography list upfront, then yields one entry at a time.
/// For entries missing `audioPlaylistId`, resolves it lazily via `GET /albums/{browseId}`.
struct YtmusicDiscographySource {
    stream: BoxStream<'static, Result<(ChildRef, ()), Error>>,
    /// Known total count once fetched; used for size_hint.
    total: Option<usize>,
    /// Items consumed so far; used to compute remaining hint.
    consumed: usize,
}

async fn fetch_discography(
    http: Arc<dyn HttpClient>,
    server_url: &str,
    channel_id: &str,
) -> Result<Vec<YtmusicRelease>, Error> {
    let url = format!(
        "{}/artists/{}/discography",
        server_url.trim_end_matches('/'),
        urlencoding::encode(channel_id)
    );
    let response = http
        .get_bytes(Request {
            url,
            ..Default::default()
        })
        .await?;
    if !response.status.is_success() {
        return Err(Error::InvalidUrl(format!(
            "ytmusicapi server returned {} for channel {channel_id}",
            response.status
        )));
    }
    let body = response
        .body_to_bytes()
        .await
        .map_err(|e| Error::InvalidUrl(e.to_string()))?;
    serde_json::from_slice(&body)
        .map_err(|e| Error::InvalidUrl(format!("ytmusicapi parse error: {e}")))
}

async fn resolve_playlist_id(
    http: Arc<dyn HttpClient>,
    server_url: &str,
    browse_id: &str,
) -> Result<String, Error> {
    let url = format!(
        "{}/albums/{}",
        server_url.trim_end_matches('/'),
        urlencoding::encode(browse_id)
    );
    let response = http
        .get_bytes(Request {
            url,
            ..Default::default()
        })
        .await?;
    if !response.status.is_success() {
        return Err(Error::InvalidUrl(format!(
            "ytmusicapi server returned {} for album {browse_id}",
            response.status
        )));
    }
    let body = response
        .body_to_bytes()
        .await
        .map_err(|e| Error::InvalidUrl(e.to_string()))?;
    let album: YtmusicAlbumResponse = serde_json::from_slice(&body)
        .map_err(|e| Error::InvalidUrl(format!("ytmusicapi album parse error: {e}")))?;
    album
        .audio_playlist_id
        .ok_or_else(|| Error::InvalidUrl(format!("no audioPlaylistId for album {browse_id}")))
}

fn discography_stream(
    http: Arc<dyn HttpClient>,
    server_url: String,
    channel_id: String,
) -> BoxStream<'static, Result<(ChildRef, ()), Error>> {
    // State: (http, server_url, channel_id, Option<pending releases>)
    // The releases are fetched lazily on the first iteration step.
    futures::stream::unfold(
        (
            http,
            server_url,
            channel_id,
            None::<std::collections::VecDeque<YtmusicRelease>>,
        ),
        |(http, server_url, channel_id, mut releases)| async move {
            if releases.is_none() {
                match fetch_discography(Arc::clone(&http), &server_url, &channel_id).await {
                    Ok(r) => releases = Some(r.into()),
                    Err(e) => return Some((Err(e), (http, server_url, channel_id, None))),
                }
            }
            let queue = releases.as_mut().unwrap();
            loop {
                let Some(release) = queue.pop_front() else {
                    return None;
                };
                let playlist_id = match release.audio_playlist_id {
                    Some(id) => id,
                    None => match release.browse_id {
                        Some(bid) => {
                            match resolve_playlist_id(Arc::clone(&http), &server_url, &bid).await {
                                Ok(id) => id,
                                Err(e) => {
                                    return Some((
                                        Err(e),
                                        (http, server_url, channel_id, releases),
                                    ));
                                }
                            }
                        }
                        None => continue,
                    },
                };
                let child = ChildRef {
                    entry_type: EntryType::Release,
                    sources: [(SOURCE.into(), HashSet::from([playlist_url(&playlist_id)]))].into(),
                    name: release.title,
                    external_type: EXTERNAL_TYPE_PLAYLIST.into(),
                    ..Default::default()
                };
                let remaining = releases.clone().map(|q| q.len());
                let _ = remaining; // used for size_hint via YtmusicDiscographySource.total
                return Some((Ok((child, ())), (http, server_url, channel_id, releases)));
            }
        },
    )
    .boxed()
}

impl YtmusicDiscographySource {
    fn new(http: Arc<dyn HttpClient>, server_url: String, channel_id: String) -> Self {
        Self {
            stream: discography_stream(http, server_url, channel_id),
            total: None,
            consumed: 0,
        }
    }
}

impl Stream for YtmusicDiscographySource {
    type Item = Result<(ChildRef, ()), Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let result = self.stream.as_mut().poll_next(cx);
        if matches!(result, Poll::Ready(Some(Ok(_)))) {
            self.consumed += 1;
        }
        result
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self.total {
            Some(n) => {
                let remaining = n.saturating_sub(self.consumed);
                (remaining, Some(remaining))
            }
            None => (0, None),
        }
    }
}

impl Unpin for YtmusicDiscographySource {}

impl ChildSource for YtmusicDiscographySource {
    fn evaluate_expr(&self, expr: &CompiledMatcherExpr) -> Tribool {
        release_eval(expr)
    }
}

// --- Static eval helpers ---

fn release_eval(expr: &CompiledMatcherExpr) -> Tribool {
    static_eval_expr(expr, &|matcher| match matcher {
        CompiledChildMatcher::EntryData(CompiledEntryDataMatcher::EntryType(t)) => {
            (*t == EntryType::Release).into()
        }
        _ => default_eval_leaf(matcher),
    })
}

fn track_eval(expr: &CompiledMatcherExpr) -> Tribool {
    static_eval_expr(expr, &|matcher| match matcher {
        CompiledChildMatcher::EntryData(CompiledEntryDataMatcher::EntryType(t)) => {
            (*t == EntryType::Track).into()
        }
        _ => default_eval_leaf(matcher),
    })
}

// --- Public API ---

pub async fn get_channel(client: &YoutubeClient, url: &str) -> Result<EntityResult<()>, Error> {
    let channel_match = match_channel_url(url).expect("Invalid YouTube channel URL");
    let url = channel_url(channel_match.kind, channel_match.id);
    let result = get_channel_raw::<ChannelListResponse, _, _, Error, _>(
        client,
        url.as_str(),
        move |c, kind, id| {
            let c = c.items.first().cloned();
            let id = id.to_string();
            let client = client.clone();
            async move {
                let id = id.as_str();
                let c =
                    c.ok_or_else(|| Error::NotFound(format!("YouTube channel not found: {id}")))?;
                let url = channel_url(kind, id);
                let channel_id = match kind {
                    ChannelKind::ChannelId => id.to_string(),
                    // The API returns the channel ID in the URL for non-ID kinds,
                    // but we need it from the response for the playlists/uploads queries.
                    // For now use the uploads playlist ID to derive it: uploads = "UU" + channel_id[2..]
                    _ => format!("UC{}", &c.content_details.related_playlists.uploads[2..]),
                };
                let uploads_playlist_id = c.content_details.related_playlists.uploads.clone();

                // Always include the canonical channel ID URL.
                let mut source_set = HashSet::from([
                    url.to_string(),
                    channel_url(ChannelKind::ChannelId, &channel_id),
                ]);
                if let Some(custom_url) = &c.snippet.custom_url {
                    if custom_url.starts_with('@') {
                        // Modern handle — only the /@handle form is valid
                        source_set.insert(channel_url(ChannelKind::Handle, custom_url));
                    } else {
                        // Legacy custom name — only the /c/name form is valid
                        source_set.insert(channel_url(ChannelKind::Custom, custom_url));
                    }
                }

                // Source 1: playlists owned by the channel (Release)
                let channel_id_for_ytmusic = channel_id.clone();
                let playlists_source =
                    PaginatedChildSource::new(Box::new(ChannelPlaylistsPageFetcher {
                        client: client.clone(),
                        channel_id,
                    }))
                    .with_static_eval(release_eval);

                // Source 2: videos uploaded by the channel (Track)
                let uploads_source =
                    PaginatedChildSource::new(Box::new(ChannelUploadsPageFetcher {
                        client: client.clone(),
                        uploads_playlist_id,
                    }))
                    .with_static_eval(track_eval);

                // Source 3 (optional): YouTube Music discography via ytmusicapi server.
                // Only added when YTMUSICAPI_SERVER_URL is configured on the client.
                let mut children: Vec<Arc<CachedChildSource>> = vec![
                    Arc::new(CachedChildSource::new(Box::new(playlists_source))),
                    Arc::new(CachedChildSource::new(Box::new(uploads_source))),
                ];
                if let Some(ytmusicapi_url) = &client.ytmusicapi_url {
                    let ytmusic_source = YtmusicDiscographySource::new(
                        client.client.clone(),
                        ytmusicapi_url.clone(),
                        channel_id_for_ytmusic,
                    );
                    children.push(Arc::new(CachedChildSource::new(Box::new(ytmusic_source))));
                }

                Ok(EntityResult {
                    release_date: None,
                    sources: [(SOURCE.into(), source_set)].into(),
                    extra: serde_json::to_value(&c).unwrap_or_default(),
                    specific_data: EntrySpecificData::Artist,
                    children,
                    aliases: vec![Alias {
                        name: c.snippet.title.clone(),
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

pub async fn get_channel_raw<T, F, R, E, FR>(
    client: &YoutubeClient,
    url: &str,
    callback: F,
) -> Result<R, E>
where
    T: std::any::Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
    F: FnOnce(&T, ChannelKind, &str) -> FR,
    E: From<Error> + Send + 'static,
    FR: Future<Output = Result<R, E>> + Send + 'static,
{
    let channel_match = match_channel_url(url).expect("Invalid YouTube channel URL");
    let (endpoint, params) = channel_endpoint_and_params(channel_match.kind, channel_match.id);
    client
        .get::<T, _, _, _, _>(
            endpoint,
            &[
                (params[0].0, params[0].1.as_str()),
                (params[1].0, params[1].1.as_str()),
            ],
            |value| callback(value, channel_match.kind, channel_match.id),
        )
        .await
}

/// Fetches the first page of playlists owned by a channel — used for fixture updates.
pub async fn get_channel_playlists_raw<T, F, R, E, FR>(
    client: &YoutubeClient,
    url: &str,
    callback: F,
) -> Result<R, E>
where
    T: std::any::Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
    F: FnOnce(&T) -> FR,
    E: From<Error> + Send + 'static,
    FR: Future<Output = Result<R, E>> + Send + 'static,
{
    // We need the numeric channel ID for the channelId param.
    // Resolve it first via the channels endpoint.
    let channel_id =
        get_channel_raw::<ChannelListResponse, _, _, E, _>(client, url, |c, kind, id| {
            let first = c.items.first().cloned();
            let id = id.to_string();
            async move {
                let channel_id = match kind {
                    ChannelKind::ChannelId => id.clone(),
                    _ => {
                        let uploads = first.ok_or_else(|| {
                            E::from(Error::NotFound(format!("YouTube channel not found: {id}")))
                        })?;
                        format!(
                            "UC{}",
                            &uploads.content_details.related_playlists.uploads[2..]
                        )
                    }
                };
                Ok(channel_id)
            }
        })
        .await?;
    client
        .get::<T, _, _, _, _>(
            "playlists",
            &[
                ("part", PLAYLISTS_PARTS),
                ("channelId", channel_id.as_str()),
                ("maxResults", PLAYLISTS_MAX_RESULTS),
            ],
            |value| callback(value),
        )
        .await
}

/// Fetches the first page of the channel's uploads playlist — used for fixture updates.
pub async fn get_channel_uploads_raw<T, F, R, E, FR>(
    client: &YoutubeClient,
    url: &str,
    callback: F,
) -> Result<R, E>
where
    T: std::any::Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
    F: FnOnce(&T) -> FR,
    E: From<Error> + Send + 'static,
    FR: Future<Output = Result<R, E>> + Send + 'static,
{
    let uploads_playlist_id =
        get_channel_raw::<ChannelListResponse, _, _, E, _>(client, url, |c, _kind, id| {
            let first = c.items.first().cloned();
            let id = id.to_string();
            async move {
                let uploads = first
                    .ok_or_else(|| {
                        E::from(Error::NotFound(format!("YouTube channel not found: {id}")))
                    })?
                    .content_details
                    .related_playlists
                    .uploads
                    .clone();
                Ok(uploads)
            }
        })
        .await?;
    client
        .get::<T, _, _, _, _>(
            "playlistItems",
            &[
                ("part", UPLOADS_PARTS),
                ("playlistId", uploads_playlist_id.as_str()),
                ("maxResults", UPLOADS_MAX_RESULTS),
            ],
            |value| callback(value),
        )
        .await
}

fn channel_endpoint_and_params(
    channel_kind: ChannelKind,
    id: &str,
) -> (&'static str, [(&'static str, String); 2]) {
    let (key, value) = match channel_kind {
        ChannelKind::ChannelId => ("id", id.to_string()),
        ChannelKind::Custom | ChannelKind::User => ("forUsername", id.to_string()),
        ChannelKind::Handle => ("forHandle", id.trim_start_matches('@').to_string()),
    };
    (
        "channels",
        [("part", CHANNEL_PARTS.to_string()), (key, value)],
    )
}

#[cfg(test)]
mod tests {
    use crate::providers::types::child_next;
    use std::{collections::HashSet, sync::Arc};

    use bytes::Bytes;
    use http::Method;

    use crate::{
        http::ResponseStatus,
        providers::{
            backends::youtube_api::{
                SOURCE,
                channel::{
                    CHANNEL_PARTS, ChannelListResponse, ChannelPlaylistsResponse,
                    PLAYLISTS_MAX_RESULTS, PLAYLISTS_PARTS, UPLOADS_MAX_RESULTS, UPLOADS_PARTS,
                    UploadsPlaylistItemsResponse, YtmusicAlbumResponse, get_channel,
                },
                client::YoutubeClient,
            },
            types::{ChildSource, EntrySpecificData, EntryType},
        },
        test_utils::MockHttpClient,
    };

    fn channel_api_url(id: &str) -> String {
        YoutubeClient::build_url("channels", &[("part", CHANNEL_PARTS), ("id", id)])
    }

    fn channel_playlists_api_url(channel_id: &str) -> String {
        YoutubeClient::build_url(
            "playlists",
            &[
                ("part", PLAYLISTS_PARTS),
                ("channelId", channel_id),
                ("maxResults", PLAYLISTS_MAX_RESULTS),
            ],
        )
    }

    fn channel_uploads_api_url(uploads_playlist_id: &str) -> String {
        YoutubeClient::build_url(
            "playlistItems",
            &[
                ("part", UPLOADS_PARTS),
                ("playlistId", uploads_playlist_id),
                ("maxResults", UPLOADS_MAX_RESULTS),
            ],
        )
    }

    #[tokio::test]
    async fn test_get_channel_with_children() -> anyhow::Result<()> {
        let channel_id = "UCqm3BQLlJfvkTsX_hvm0UmA";
        let uploads_playlist_id = "UUqm3BQLlJfvkTsX_hvm0UmA";
        let api_key = "doesnotmatter";

        let mut http_client = MockHttpClient::new();
        http_client.add_route_json::<ChannelListResponse>(
            Method::GET,
            &channel_api_url(channel_id),
            include_str!("./channel_watame.json"),
        );
        http_client.add_route_json::<ChannelPlaylistsResponse>(
            Method::GET,
            &channel_playlists_api_url(channel_id),
            include_str!("./channel_watame_playlists_page1.json"),
        );
        http_client.add_route_json::<UploadsPlaylistItemsResponse>(
            Method::GET,
            &channel_uploads_api_url(uploads_playlist_id),
            include_str!("./channel_watame_uploads_page1.json"),
        );

        let client = YoutubeClient::new_with_client(Arc::new(http_client), api_key.to_string())?;

        let channel = get_channel(
            &client,
            &format!("https://www.youtube.com/channel/{channel_id}"),
        )
        .await?;

        // Basic metadata
        assert!(matches!(channel.specific_data, EntrySpecificData::Artist));
        assert_eq!(channel.aliases.len(), 1);
        assert_eq!(channel.aliases[0].name, "Watame Ch. 角巻わため");
        assert!(channel.aliases[0].primary);

        // Sources: channel ID URL + custom URL
        assert_eq!(
            channel.sources.get(SOURCE).unwrap(),
            &HashSet::from([
                format!("https://www.youtube.com/channel/{channel_id}"),
                "https://www.youtube.com/@tsunomakiwatame".to_string(),
            ])
        );

        // Two child sources: playlists (Release) and uploads (Track)
        assert_eq!(channel.children.len(), 2);

        // Source 0: playlists → Release entries (only read first page's first item;
        // the fixture has a nextPageToken so we don't drain beyond it)
        let mut playlists_cursor = channel.children[0].cursor();
        let (first_playlist, _) = child_next(&mut playlists_cursor)
            .await?
            .expect("expected a playlist");
        assert_eq!(first_playlist.entry_type, EntryType::Release);
        assert_eq!(
            first_playlist.sources.get(SOURCE).unwrap(),
            &HashSet::from([
                "https://www.youtube.com/playlist?list=PLZ34fLWik_iAHbPlz6em1t6U8rb75DB_K"
                    .to_string()
            ])
        );
        assert_eq!(
            first_playlist.name.as_deref().unwrap(),
            "ぷちホロの村 - 剣とお店と田舎暮らし🐏"
        );

        // Source 1: uploads → Track entries (same: only read the first item)
        let mut uploads_cursor = channel.children[1].cursor();
        let (first_upload, _) = child_next(&mut uploads_cursor)
            .await?
            .expect("expected an upload");
        assert_eq!(first_upload.entry_type, EntryType::Track);
        assert_eq!(
            first_upload.sources.get(SOURCE).unwrap(),
            &HashSet::from(["https://youtu.be/3MrxDLj2fOw".to_string()])
        );
        assert_eq!(
            first_upload.name.as_deref().unwrap(),
            "【雑談＆お礼】新衣装だったりガンダムだったり嬉しいね！【角巻わため/ホロライブ４期生】"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_get_channel_with_ytmusicapi() -> anyhow::Result<()> {
        let channel_id = "UCqm3BQLlJfvkTsX_hvm0UmA";
        let uploads_playlist_id = "UUqm3BQLlJfvkTsX_hvm0UmA";
        let api_key = "doesnotmatter";
        let ytmusicapi_url = "http://localhost:9001";

        let mut http_client = MockHttpClient::new();
        http_client.add_route_json::<ChannelListResponse>(
            Method::GET,
            &channel_api_url(channel_id),
            include_str!("./channel_watame.json"),
        );
        http_client.add_route_json::<ChannelPlaylistsResponse>(
            Method::GET,
            &channel_playlists_api_url(channel_id),
            include_str!("./channel_watame_playlists_page1.json"),
        );
        http_client.add_route_json::<UploadsPlaylistItemsResponse>(
            Method::GET,
            &channel_uploads_api_url(uploads_playlist_id),
            include_str!("./channel_watame_uploads_page1.json"),
        );
        // ytmusicapi server route — artist overview
        let ytmusic_url = format!(
            "{}/artists/{}",
            ytmusicapi_url,
            urlencoding::encode(channel_id)
        );
        http_client.add_route(
            Method::GET,
            &ytmusic_url,
            ResponseStatus::OK,
            Bytes::from_static(include_bytes!("./ytmusic_watame_artist.json")),
        );
        // discography route
        let ytmusic_discography_url = format!(
            "{}/artists/{}/discography",
            ytmusicapi_url,
            urlencoding::encode(channel_id),
        );
        http_client.add_route(
            Method::GET,
            &ytmusic_discography_url,
            ResponseStatus::OK,
            Bytes::from_static(include_bytes!("./ytmusic_watame_discography.json")),
        );
        // one single album resolution route (DivaFever)
        let divafever_browse_id = "MPREb_Rd27MfU0AZG";
        http_client.add_route_json::<YtmusicAlbumResponse>(
            Method::GET,
            &format!(
                "{}/albums/{}",
                ytmusicapi_url,
                urlencoding::encode(divafever_browse_id)
            ),
            include_str!("./ytmusic_divafever_album.json"),
        );

        let client = YoutubeClient::new_with_client(Arc::new(http_client), api_key.to_string())?
            .with_ytmusicapi_url(ytmusicapi_url.to_string());

        let channel = get_channel(
            &client,
            &format!("https://www.youtube.com/channel/{channel_id}"),
        )
        .await?;

        // Three child sources when ytmusicapi is configured
        assert_eq!(channel.children.len(), 3);

        // Source 2: ytmusicapi discography
        let mut ytmusic_cursor = channel.children[2].cursor();
        // First next() triggers the fetch (22 entries in the discography list).
        let first = child_next(&mut ytmusic_cursor)
            .await?
            .expect("expected first release");
        // Spot-check first release (Hop Step Sheep — has audioPlaylistId directly)
        assert_eq!(first.0.entry_type, EntryType::Release);
        assert_eq!(first.0.name.as_deref().unwrap(), "Hop Step Sheep");
        assert_eq!(
            first.0.sources.get(SOURCE).unwrap(),
            &HashSet::from([
                "https://www.youtube.com/playlist?list=OLAK5uy_kZg-epuqXBYiWa_PrltWZXm7OtfRdiUgE"
                    .to_string()
            ])
        );
        // Second and third albums
        let second = child_next(&mut ytmusic_cursor)
            .await?
            .expect("expected second release");
        assert_eq!(second.0.name.as_deref().unwrap(), "わためのうた vol.２");
        let third = child_next(&mut ytmusic_cursor)
            .await?
            .expect("expected third release");
        assert_eq!(third.0.name.as_deref().unwrap(), "WATAME NO UTA vol.1");
        // Fourth: DivaFever — resolved lazily via /albums/{browseId}
        let fourth = child_next(&mut ytmusic_cursor)
            .await?
            .expect("expected DivaFever");
        assert_eq!(fourth.0.name.as_deref().unwrap(), "DivaFever");
        assert_eq!(
            fourth.0.sources.get(SOURCE).unwrap(),
            &HashSet::from([
                "https://www.youtube.com/playlist?list=OLAK5uy_kUTSWTOJ17BRGsluouawuOnmnJFRRSK2o"
                    .to_string()
            ])
        );

        Ok(())
    }
}
