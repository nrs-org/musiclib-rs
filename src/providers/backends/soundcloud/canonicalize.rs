use super::{EXTERNAL_TYPE_ARTIST, EXTERNAL_TYPE_PLAYLIST, EXTERNAL_TYPE_TRACK, SOURCE};
use crate::providers::std_values::StandardProviderKeys;
use crate::providers::types::{CanonicalizeResult, EntryType};

fn strip_query(url: &str) -> &str {
    url.split_once('?').map(|(base, _)| base).unwrap_or(url)
}

fn sc_rest(url: &str) -> Option<&str> {
    let base = strip_query(url);
    base.strip_prefix("https://soundcloud.com/")
        .or_else(|| base.strip_prefix("http://soundcloud.com/"))
}

/// e.g. https://soundcloud.com/laserimouto/prismatix
pub fn match_track_url(url: &str) -> Option<String> {
    let rest = sc_rest(url)?;
    let mut parts = rest.splitn(3, '/');
    let user = parts.next().filter(|s| !s.is_empty())?;
    let slug = parts.next().filter(|s| !s.is_empty() && *s != "sets")?;
    if parts.next().is_some() {
        return None;
    }
    Some(format!("https://soundcloud.com/{user}/{slug}"))
}

/// e.g. https://soundcloud.com/laserimouto/sets/anime-hardcore-bootleg
pub fn match_playlist_url(url: &str) -> Option<String> {
    let rest = sc_rest(url)?;
    let mut parts = rest.splitn(4, '/');
    let user = parts.next().filter(|s| !s.is_empty())?;
    let sets = parts.next().filter(|s| *s == "sets")?;
    let slug = parts.next().filter(|s| !s.is_empty())?;
    Some(format!("https://soundcloud.com/{user}/{sets}/{slug}"))
}

/// e.g. https://soundcloud.com/laserimouto
pub fn match_artist_url(url: &str) -> Option<String> {
    let rest = sc_rest(url)?;
    let mut parts = rest.splitn(2, '/');
    let user = parts.next().filter(|s| !s.is_empty())?;
    if parts.next().filter(|s| !s.is_empty()).is_some() {
        return None;
    }
    Some(format!("https://soundcloud.com/{user}"))
}

/// Returns the sets listing URL for a canonical artist URL.
/// e.g. https://soundcloud.com/laserimouto → https://soundcloud.com/laserimouto/sets
pub fn artist_sets_url(artist_url: &str) -> String {
    format!("{}/sets", artist_url.trim_end_matches('/'))
}

/// Returns the albums listing URL for a canonical artist URL.
/// e.g. https://soundcloud.com/laserimouto → https://soundcloud.com/laserimouto/albums
pub fn artist_albums_url(artist_url: &str) -> String {
    format!("{}/albums", artist_url.trim_end_matches('/'))
}

pub fn canonicalize(source_key: &str, identifier: &str) -> Option<CanonicalizeResult> {
    if source_key != StandardProviderKeys::UNKNOWN_URL {
        return None;
    }
    if let Some(id) = match_track_url(identifier) {
        return Some(CanonicalizeResult {
            canonical_source_key: SOURCE.into(),
            canonical_identifier: id,
            entry_type: EntryType::Track,
            external_type: EXTERNAL_TYPE_TRACK.into(),
        });
    }
    if let Some(id) = match_playlist_url(identifier) {
        return Some(CanonicalizeResult {
            canonical_source_key: SOURCE.into(),
            canonical_identifier: id,
            entry_type: EntryType::Release,
            external_type: EXTERNAL_TYPE_PLAYLIST.into(),
        });
    }
    if let Some(id) = match_artist_url(identifier) {
        return Some(CanonicalizeResult {
            canonical_source_key: SOURCE.into(),
            canonical_identifier: id,
            entry_type: EntryType::Artist,
            external_type: EXTERNAL_TYPE_ARTIST.into(),
        });
    }
    None
}

use crate::providers::CanonicalizeProvider;
use async_trait::async_trait;

pub struct Canonicalizer;

#[async_trait]
impl CanonicalizeProvider for Canonicalizer {
    async fn canonicalize(&self, source_key: &str, identifier: &str) -> Option<CanonicalizeResult> {
        canonicalize(source_key, identifier)
    }
}
