use std::sync::LazyLock;

use regex::Regex;

use crate::providers::types::{CanonicalizeResult, EntryType, TrackPosition};

use super::SOURCE;
use super::types::{
    EXTERNAL_TYPE_ARTIST, EXTERNAL_TYPE_MASTER, EXTERNAL_TYPE_RELEASE, EXTERNAL_TYPE_TRACK,
};
use crate::providers::std_values::StandardProviderKeys;

macro_rules! discogs_url_regex {
    ($entity:literal) => {
        LazyLock::new(|| {
            Regex::new(&format!(
                r"^https?://(?:www\.)?discogs\.com/{}/(\d+)(?:[/?#-].*)?$",
                $entity
            ))
            .unwrap()
        })
    };
}

static RELEASE_RE: LazyLock<Regex> = discogs_url_regex!("release");
static MASTER_RE: LazyLock<Regex> = discogs_url_regex!("master");
static ARTIST_RE: LazyLock<Regex> = discogs_url_regex!("artist");

// Matches pseudo-URLs like:
//   https://www.discogs.com/release/123?track=5      (single disc)
//   https://www.discogs.com/release/123?track=2-1    (multi-disc)
static TRACK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^https?://(?:www\.)?discogs\.com/release/(\d+)\?track=(\d+)(?:-(\d+))?$").unwrap()
});

fn match_url<'a>(re: &Regex, url: &'a str) -> Option<&'a str> {
    re.captures(url)
        .and_then(|c| c.get(1))
        .map(|m| &url[m.start()..m.end()])
}

pub fn match_release_url(url: &str) -> Option<&str> {
    // A track URL also contains a release ID — check for track first so release
    // matching doesn't swallow track pseudo-URLs.
    if TRACK_RE.is_match(url) {
        return None;
    }
    match_url(&RELEASE_RE, url)
}

pub fn match_master_url(url: &str) -> Option<&str> {
    match_url(&MASTER_RE, url)
}

pub fn match_artist_url(url: &str) -> Option<&str> {
    match_url(&ARTIST_RE, url)
}

/// Returns `(release_id, TrackPosition)` for a track pseudo-URL, or `None`.
pub fn match_track_url(url: &str) -> Option<(String, TrackPosition)> {
    let caps = TRACK_RE.captures(url)?;
    let release_id = caps.get(1)?.as_str().to_string();
    let a: i32 = caps.get(2)?.as_str().parse().ok()?;
    let position = if let Some(b) = caps.get(3) {
        // ?track=disc-track
        let track_no: i32 = b.as_str().parse().ok()?;
        TrackPosition {
            disc_no: Some(a),
            track_no,
        }
    } else {
        // ?track=track
        TrackPosition {
            disc_no: None,
            track_no: a,
        }
    };
    Some((release_id, position))
}

pub fn release_url(id: &str) -> String {
    format!("https://www.discogs.com/release/{id}")
}

pub fn master_url(id: &str) -> String {
    format!("https://www.discogs.com/master/{id}")
}

pub fn artist_url(id: &str) -> String {
    format!("https://www.discogs.com/artist/{id}")
}

pub fn track_url(release_id: &str, position: &TrackPosition) -> String {
    match position.disc_no {
        None => format!(
            "https://www.discogs.com/release/{release_id}?track={}",
            position.track_no
        ),
        Some(disc) => format!(
            "https://www.discogs.com/release/{release_id}?track={disc}-{}",
            position.track_no
        ),
    }
}

pub fn canonicalize(source_key: &str, identifier: &str) -> Option<CanonicalizeResult> {
    if source_key != StandardProviderKeys::UNKNOWN_URL {
        return None;
    }
    if let Some((release_id, position)) = match_track_url(identifier) {
        let canonical = track_url(&release_id, &position);
        return Some(CanonicalizeResult {
            canonical_source_key: SOURCE.into(),
            canonical_identifier: canonical,
            entry_type: EntryType::Track,
            external_type: EXTERNAL_TYPE_TRACK.into(),
        });
    }
    if let Some(id) = match_release_url(identifier) {
        return Some(CanonicalizeResult {
            canonical_source_key: SOURCE.into(),
            canonical_identifier: release_url(id),
            entry_type: EntryType::Release,
            external_type: EXTERNAL_TYPE_RELEASE.into(),
        });
    }
    if let Some(id) = match_master_url(identifier) {
        return Some(CanonicalizeResult {
            canonical_source_key: SOURCE.into(),
            canonical_identifier: master_url(id),
            entry_type: EntryType::ReleaseGroup,
            external_type: EXTERNAL_TYPE_MASTER.into(),
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

use crate::providers::CanonicalizeProvider;
use async_trait::async_trait;

pub struct Canonicalizer;

#[async_trait]
impl CanonicalizeProvider for Canonicalizer {
    async fn canonicalize(&self, source_key: &str, identifier: &str) -> Option<CanonicalizeResult> {
        canonicalize(source_key, identifier)
    }
}
