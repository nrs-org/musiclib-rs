use crate::providers::types::{CanonicalizeResult, EntryType};

use super::{
    EXTERNAL_TYPE_ARTIST, EXTERNAL_TYPE_MYLIST, EXTERNAL_TYPE_SERIES, EXTERNAL_TYPE_VIDEO, SOURCE,
};
use crate::providers::std_values::StandardProviderKeys;

/// Matches `https://www.nicovideo.jp/watch/smXXXX` etc.
pub fn match_video_url(url: &str) -> Option<&str> {
    let path = url
        .strip_prefix("https://www.nicovideo.jp/watch/")?
        .split('?')
        .next()?;
    if path.is_empty() { None } else { Some(path) }
}

/// Matches `https://www.nicovideo.jp/user/XXXXX` (with or without trailing path/query).
pub fn match_user_url(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://www.nicovideo.jp/user/")?;
    let id = rest.split(['/', '?', '#']).next()?;
    if id.is_empty() { None } else { Some(id) }
}

pub fn user_url(user_id: &str) -> String {
    format!("https://www.nicovideo.jp/user/{user_id}")
}

/// Matches `https://www.nicovideo.jp/user/XXXXX/mylist/YYYYY`.
pub fn match_mylist_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("https://www.nicovideo.jp/user/")?;
    let (user_id, rest) = rest.split_once("/mylist/")?;
    let mylist_id = rest.split(['?', '#']).next()?;
    if user_id.is_empty() || mylist_id.is_empty() {
        None
    } else {
        Some((user_id, mylist_id))
    }
}

pub fn mylist_url(user_id: &str, mylist_id: &str) -> String {
    format!("https://www.nicovideo.jp/user/{user_id}/mylist/{mylist_id}")
}

/// Matches `https://www.nicovideo.jp/series/XXXXX`.
pub fn match_series_url(url: &str) -> Option<&str> {
    let id = url
        .strip_prefix("https://www.nicovideo.jp/series/")?
        .split(['?', '#'])
        .next()?;
    if id.is_empty() { None } else { Some(id) }
}

pub fn canonicalize(source_key: &str, identifier: &str) -> Option<CanonicalizeResult> {
    if source_key != StandardProviderKeys::UNKNOWN_URL {
        return None;
    }
    if let Some(id) = match_video_url(identifier) {
        return Some(CanonicalizeResult {
            canonical_source_key: SOURCE.into(),
            canonical_identifier: format!("https://www.nicovideo.jp/watch/{id}"),
            entry_type: EntryType::Track,
            external_type: EXTERNAL_TYPE_VIDEO.into(),
        });
    }
    if let Some(id) = match_series_url(identifier) {
        return Some(CanonicalizeResult {
            canonical_source_key: SOURCE.into(),
            canonical_identifier: format!("https://www.nicovideo.jp/series/{id}"),
            entry_type: EntryType::Release,
            external_type: EXTERNAL_TYPE_SERIES.into(),
        });
    }
    if let Some((user_id, mylist_id)) = match_mylist_url(identifier) {
        return Some(CanonicalizeResult {
            canonical_source_key: SOURCE.into(),
            canonical_identifier: mylist_url(user_id, mylist_id),
            entry_type: EntryType::Release,
            external_type: EXTERNAL_TYPE_MYLIST.into(),
        });
    }
    if let Some(id) = match_user_url(identifier) {
        return Some(CanonicalizeResult {
            canonical_source_key: SOURCE.into(),
            canonical_identifier: user_url(id),
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
