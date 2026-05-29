use std::sync::LazyLock;

use regex::Regex;

use crate::providers::types::{CanonicalizeResult, EntryType};

use super::SOURCE;
use super::types::{
    EXTERNAL_TYPE_ALBUM, EXTERNAL_TYPE_ARTIST, EXTERNAL_TYPE_PLAYLIST, EXTERNAL_TYPE_TRACK,
};
use crate::providers::std_values::StandardProviderKeys;

const SPOTIFY_ID_PATTERN: &str = r"[0-9A-Za-z]{22}";

macro_rules! spotify_url_regex {
    ($entity:literal) => {
        LazyLock::new(|| {
            Regex::new(&format!(
                r"^https?://open\.spotify\.com/{}/({})(?:[/?#].*)?$",
                $entity, SPOTIFY_ID_PATTERN
            ))
            .unwrap()
        })
    };
}

static TRACK_RE: LazyLock<Regex> = spotify_url_regex!("track");
static ALBUM_RE: LazyLock<Regex> = spotify_url_regex!("album");
static PLAYLIST_RE: LazyLock<Regex> = spotify_url_regex!("playlist");
static ARTIST_RE: LazyLock<Regex> = spotify_url_regex!("artist");

fn match_url<'a>(re: &Regex, url: &'a str) -> Option<&'a str> {
    re.captures(url)
        .and_then(|c| c.get(1))
        .map(|m| &url[m.start()..m.end()])
}

pub fn match_track_url(url: &str) -> Option<&str> {
    match_url(&TRACK_RE, url)
}

pub fn match_album_url(url: &str) -> Option<&str> {
    match_url(&ALBUM_RE, url)
}

pub fn match_playlist_url(url: &str) -> Option<&str> {
    match_url(&PLAYLIST_RE, url)
}

pub fn match_artist_url(url: &str) -> Option<&str> {
    match_url(&ARTIST_RE, url)
}

pub fn track_url(id: &str) -> String {
    format!("https://open.spotify.com/track/{id}")
}

pub fn album_url(id: &str) -> String {
    format!("https://open.spotify.com/album/{id}")
}

pub fn playlist_url(id: &str) -> String {
    format!("https://open.spotify.com/playlist/{id}")
}

pub fn artist_url(id: &str) -> String {
    format!("https://open.spotify.com/artist/{id}")
}

pub fn canonicalize(source_key: &str, identifier: &str) -> Option<CanonicalizeResult> {
    if source_key != StandardProviderKeys::UNKNOWN_URL {
        return None;
    }
    if let Some(id) = match_track_url(identifier) {
        return Some(CanonicalizeResult {
            canonical_source_key: SOURCE.into(),
            canonical_identifier: track_url(id),
            entry_type: EntryType::Track,
            external_type: EXTERNAL_TYPE_TRACK.into(),
        });
    }
    if let Some(id) = match_album_url(identifier) {
        return Some(CanonicalizeResult {
            canonical_source_key: SOURCE.into(),
            canonical_identifier: album_url(id),
            entry_type: EntryType::Release,
            external_type: EXTERNAL_TYPE_ALBUM.into(),
        });
    }
    if let Some(id) = match_playlist_url(identifier) {
        return Some(CanonicalizeResult {
            canonical_source_key: SOURCE.into(),
            canonical_identifier: playlist_url(id),
            entry_type: EntryType::Release,
            external_type: EXTERNAL_TYPE_PLAYLIST.into(),
        });
    }
    if let Some(id) = match_artist_url(identifier) {
        return Some(CanonicalizeResult {
            canonical_source_key: SOURCE.into(),
            canonical_identifier: artist_url(id),
            entry_type: EntryType::Artist,
            external_type: EXTERNAL_TYPE_ARTIST.into(),
        });
    }
    None
}
