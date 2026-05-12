use async_trait::async_trait;

use crate::{
    httpcache::{CanonicalizeProvider, FetchProvider, RawFetchProvider},
    providers::{
        backends::youtube_api::{
            canonicalize::{
                channel_url, match_channel_url, match_playlist_url, match_video_url, playlist_url,
                video_url,
            },
            channel::get_channel,
            client::YoutubeClient,
            playlist::get_playlist,
            video::get_video,
        },
        types::{CanonicalizeResult, EntityResult, EntryType, Error},
    },
};

pub enum StringFilterMode {
    Include,
    Exclude,
}

pub struct StringFilter {
    pattern: String,
    priority: i32,
    mode: StringFilterMode,
}

pub struct StringFilterSet {
    filters: Vec<StringFilter>,
}

pub struct DurationRange {
    min: Option<u64>,
    max: Option<u64>,
}

pub struct VideoDiscographyFetchOptions {
    // true -> only fetch videos with category 10 (Music)
    // (this is not really reliable)
    filter_music_category: bool,

    title_filters: StringFilterSet,
    description_filters: StringFilterSet,
    duration_range: Option<DurationRange>,
}

pub struct PlaylistDiscographyFetchOptions {
    title_filters: StringFilterSet,
    description_filters: StringFilterSet,
}

pub struct YtMusicDiscographyFetchOptions {}

pub struct DiscographyFetchOptions {
    videos: Option<VideoDiscographyFetchOptions>,
    playlists: Option<PlaylistDiscographyFetchOptions>,
    ytmusic: Option<YtMusicDiscographyFetchOptions>,
}

#[derive(Default)]
pub struct EntryFetchOptions {
    discography: Option<DiscographyFetchOptions>,
}

pub struct YoutubeProvider {
    client: YoutubeClient,
}

impl YoutubeProvider {
    pub fn new(api_key: String) -> Result<Self, Error> {
        Ok(Self {
            client: YoutubeClient::new(api_key)?,
        })
    }
}

#[async_trait]
impl CanonicalizeProvider for YoutubeProvider {
    async fn canonicalize(&self, url: &str) -> Option<CanonicalizeResult> {
        if let Some(id) = match_video_url(url) {
            return Some(CanonicalizeResult {
                canonical_identifier: video_url(id),
                entry_type: EntryType::Track,
                external_type: EXTERNAL_TYPE_VIDEO.into(),
            });
        }

        if let Some(id) = match_playlist_url(url) {
            return Some(CanonicalizeResult {
                canonical_identifier: playlist_url(id),
                entry_type: EntryType::Release,
                external_type: EXTERNAL_TYPE_PLAYLIST.into(),
            });
        }

        if let Some(channel_match) = match_channel_url(url) {
            return Some(CanonicalizeResult {
                canonical_identifier: channel_url(channel_match.kind, channel_match.id),
                entry_type: EntryType::Artist,
                external_type: EXTERNAL_TYPE_CHANNEL.into(),
            });
        }

        None
    }
}

#[async_trait]
impl FetchProvider for YoutubeProvider {
    async fn fetch_entry(
        &self,
        identifier: &str,
        _fetch_options: crate::providers::types::EntryFetchOptions,
    ) -> Result<EntityResult, crate::providers::types::Error> {
        if match_video_url(identifier).is_some() {
            return get_video(&self.client, identifier).await;
        }
        if match_playlist_url(identifier).is_some() {
            return get_playlist(&self.client, identifier).await;
        }
        if match_channel_url(identifier).is_some() {
            return get_channel(&self.client, identifier).await;
        }

        Err(Error::MissingCredentials("Invalid YouTube URL".to_string()))
    }
}

#[async_trait]
impl RawFetchProvider for YoutubeProvider {}

pub const EXTERNAL_TYPE_VIDEO: &str = "video";
pub const EXTERNAL_TYPE_PLAYLIST: &str = "playlist";
pub const EXTERNAL_TYPE_CHANNEL: &str = "channel";
