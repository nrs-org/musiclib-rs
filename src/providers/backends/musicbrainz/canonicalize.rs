use std::sync::LazyLock;

use regex::Regex;

use crate::providers::types::{CanonicalizeResult, EntryType};

use super::SOURCE;
use super::types::{
    EXTERNAL_TYPE_ARTIST, EXTERNAL_TYPE_RECORDING, EXTERNAL_TYPE_RELEASE,
    EXTERNAL_TYPE_RELEASE_GROUP,
};
use crate::providers::std_values::StandardProviderKeys;

const MBID_PATTERN: &str = r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}";

macro_rules! mb_url_regex {
    ($entity:literal) => {
        LazyLock::new(|| {
            Regex::new(&format!(
                r"^https?://(?:www\.)?musicbrainz\.org/{}/({})(?:[/?#].*)?$",
                $entity, MBID_PATTERN
            ))
            .unwrap()
        })
    };
}

static ARTIST_RE: LazyLock<Regex> = mb_url_regex!("artist");
static RELEASE_GROUP_RE: LazyLock<Regex> = mb_url_regex!("release-group");
static RELEASE_RE: LazyLock<Regex> = mb_url_regex!("release");
static RECORDING_RE: LazyLock<Regex> = mb_url_regex!("recording");

fn match_url<'a>(re: &Regex, url: &'a str) -> Option<&'a str> {
    re.captures(url)
        .and_then(|c| c.get(1))
        .map(|m| &url[m.start()..m.end()])
}

pub fn match_artist_url(url: &str) -> Option<&str> {
    match_url(&ARTIST_RE, url)
}

pub fn match_release_group_url(url: &str) -> Option<&str> {
    match_url(&RELEASE_GROUP_RE, url)
}

pub fn match_release_url(url: &str) -> Option<&str> {
    match_url(&RELEASE_RE, url)
}

pub fn match_recording_url(url: &str) -> Option<&str> {
    match_url(&RECORDING_RE, url)
}

pub fn artist_url(mbid: &str) -> String {
    format!("https://musicbrainz.org/artist/{mbid}")
}

pub fn release_group_url(mbid: &str) -> String {
    format!("https://musicbrainz.org/release-group/{mbid}")
}

pub fn release_url(mbid: &str) -> String {
    format!("https://musicbrainz.org/release/{mbid}")
}

pub fn recording_url(mbid: &str) -> String {
    format!("https://musicbrainz.org/recording/{mbid}")
}

pub fn canonicalize(source_key: &str, identifier: &str) -> Option<CanonicalizeResult> {
    if source_key != StandardProviderKeys::UNKNOWN_URL && source_key != SOURCE {
        return None;
    }
    if let Some(mbid) = match_recording_url(identifier) {
        return Some(CanonicalizeResult {
            canonical_source_key: SOURCE.into(),
            canonical_identifier: recording_url(mbid),
            entry_type: EntryType::Track,
            external_type: EXTERNAL_TYPE_RECORDING.into(),
        });
    }
    if let Some(mbid) = match_release_url(identifier) {
        return Some(CanonicalizeResult {
            canonical_source_key: SOURCE.into(),
            canonical_identifier: release_url(mbid),
            entry_type: EntryType::Release,
            external_type: EXTERNAL_TYPE_RELEASE.into(),
        });
    }
    if let Some(mbid) = match_release_group_url(identifier) {
        return Some(CanonicalizeResult {
            canonical_source_key: SOURCE.into(),
            canonical_identifier: release_group_url(mbid),
            entry_type: EntryType::ReleaseGroup,
            external_type: EXTERNAL_TYPE_RELEASE_GROUP.into(),
        });
    }
    if let Some(mbid) = match_artist_url(identifier) {
        return Some(CanonicalizeResult {
            canonical_source_key: SOURCE.into(),
            canonical_identifier: artist_url(mbid),
            entry_type: EntryType::Artist,
            external_type: EXTERNAL_TYPE_ARTIST.into(),
        });
    }
    None
}

#[cfg(test)]
mod roundtrip_tests {
    use super::*;

    fn via_unknown(url: &str) -> Option<CanonicalizeResult> {
        canonicalize(StandardProviderKeys::UNKNOWN_URL, url)
    }

    fn via_source(canonical_id: &str) -> Option<CanonicalizeResult> {
        canonicalize(SOURCE, canonical_id)
    }

    const MBID: &str = "ab4266ab-0e5c-4a2d-9bb4-b3a416efca3e";

    #[test]
    fn recording_roundtrip() {
        let url = format!("https://musicbrainz.org/recording/{MBID}");
        let via_url = via_unknown(&url).unwrap();
        let via_canon = via_source(&via_url.canonical_identifier).unwrap();
        assert_eq!(via_url, via_canon);
    }

    #[test]
    fn release_roundtrip() {
        let url = format!("https://musicbrainz.org/release/{MBID}");
        let via_url = via_unknown(&url).unwrap();
        let via_canon = via_source(&via_url.canonical_identifier).unwrap();
        assert_eq!(via_url, via_canon);
    }

    #[test]
    fn release_group_roundtrip() {
        let url = format!("https://musicbrainz.org/release-group/{MBID}");
        let via_url = via_unknown(&url).unwrap();
        let via_canon = via_source(&via_url.canonical_identifier).unwrap();
        assert_eq!(via_url, via_canon);
    }

    #[test]
    fn artist_roundtrip() {
        let url = format!("https://musicbrainz.org/artist/{MBID}");
        let via_url = via_unknown(&url).unwrap();
        let via_canon = via_source(&via_url.canonical_identifier).unwrap();
        assert_eq!(via_url, via_canon);
    }

    #[test]
    fn foreign_source_key_rejected() {
        assert!(
            canonicalize(
                "youtube",
                &format!("https://musicbrainz.org/recording/{MBID}")
            )
            .is_none()
        );
    }
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
