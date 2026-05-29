use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::providers::std_values::StandardProviderKeys;
use crate::providers::{
    backends::musicbrainz::{
        SOURCE,
        canonicalize::{artist_url, recording_url, release_group_url, release_url},
        client::MusicBrainzClient,
    },
    types::{Error, ExternalSources},
};

// --- Shared types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UrlResource {
    pub resource: String,
}

// --- Helpers ---

/// Map a URL to a source key string. Returns `StandardProviderKeys::UNKNOWN_URL` for unrecognised domains.
pub(crate) fn url_source_key(url: &str) -> &'static str {
    let Some(host) = url
        .split("//")
        .nth(1)
        .and_then(|s| s.split('/').next())
        .map(|h| h.trim_start_matches("www."))
    else {
        return StandardProviderKeys::UNKNOWN_URL;
    };
    match host {
        "youtube.com" | "youtu.be" | "music.youtube.com" => "youtube",
        "open.spotify.com" => "spotify",
        "discogs.com" => "discogs",
        "soundcloud.com" => "soundcloud",
        "nicovideo.jp" => "nicovideo",
        "music.apple.com" | "itunes.apple.com" => "apple_music",
        "tidal.com" => "tidal",
        "deezer.com" => "deezer",
        "vgmdb.net" => "vgmdb",
        "last.fm" | "lastfm.com" => "lastfm",
        _ => StandardProviderKeys::UNKNOWN_URL,
    }
}

// --- API response types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UrlResponse {
    pub id: String,
    pub resource: String,
    #[serde(default)]
    pub relations: Vec<UrlRelation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UrlRelation {
    #[serde(rename = "target-type")]
    pub target_type: String,
    pub artist: Option<UrlRelationEntity>,
    pub recording: Option<UrlRelationEntity>,
    pub release: Option<UrlRelationEntity>,
    #[serde(rename = "release-group")]
    pub release_group: Option<UrlRelationEntity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UrlRelationEntity {
    pub id: String,
}

// --- Constants ---

const PARTS: &str = "artist-rels+recording-rels+release-rels+release-group-rels";

// --- Public API ---

/// Look up a single external URL in MusicBrainz and return any linked MB entity URLs.
pub async fn lookup_url(
    client: &MusicBrainzClient,
    resource_url: &str,
) -> Result<Option<ExternalSources>, Error> {
    let result = client
        .get::<UrlResponse, _, _, Error, _>(
            "url",
            &[("resource", resource_url), ("inc", PARTS)],
            |resp| {
                let mb_urls: HashSet<String> = resp
                    .relations
                    .iter()
                    .filter_map(|rel| match rel.target_type.as_str() {
                        "artist" => rel.artist.as_ref().map(|e| artist_url(&e.id)),
                        "recording" => rel.recording.as_ref().map(|e| recording_url(&e.id)),
                        "release" => rel.release.as_ref().map(|e| release_url(&e.id)),
                        "release-group" => {
                            rel.release_group.as_ref().map(|e| release_group_url(&e.id))
                        }
                        _ => None,
                    })
                    .collect();
                async move {
                    Ok(if mb_urls.is_empty() {
                        None
                    } else {
                        Some(ExternalSources::from([(SOURCE.into(), mb_urls)]))
                    })
                }
            },
        )
        .await;
    match result {
        Ok(found) => Ok(found),
        Err(Error::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}
