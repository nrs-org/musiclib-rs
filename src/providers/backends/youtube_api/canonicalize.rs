use fancy_regex::Regex;
use std::sync::LazyLock;

use crate::providers::backends::youtube_api::SOURCE;
use crate::providers::backends::youtube_api::types::{
    EXTERNAL_TYPE_CHANNEL_ID, EXTERNAL_TYPE_CUSTOM_CHANNEL, EXTERNAL_TYPE_HANDLE,
    EXTERNAL_TYPE_PLAYLIST, EXTERNAL_TYPE_USER_CHANNEL, EXTERNAL_TYPE_VIDEO,
};
use crate::providers::std_values::StandardProviderKeys;
use crate::providers::types::CanonicalizeResult;

// ─────────────────────────────────────────────
// 1. VIDEO URL
// ─────────────────────────────────────────────

static VIDEO_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?xi)
        ^
        (?:
            # Standard youtube.com paths
            https?://
            (?:\w+\.)?youtube(?:-nocookie|kids)?\.com/
            (?:
                (?:v|embed|e|shorts|live)/(?!videoseries|live_stream)
                |
                (?:
                    (?:(?:watch|movie)(?:_popup)?(?:\.php)?/?)?
                    (?:\?|\#!?)
                    (?:.*?[&;])??
                    v=
                )
            )
            |
            # Short youtu.be
            https?://youtu\.be/
            |
            # music.youtube.com
            https?://music\.youtube\.com/watch\?(?:.*?[&;])??v=
        )
        (?P<id>[0-9A-Za-z_\-]{11})
        (?:[?\#&].*)?
        $
        ",
    )
    .expect("invalid VIDEO_URL_RE")
});

/// Return the video ID if `url` is a YouTube video page, else `None`.
pub fn match_video_url(url: &str) -> Option<&str> {
    let caps = VIDEO_URL_RE.captures(url).ok().flatten()?;
    caps.name("id").map(|m| m.as_str())
}

// ─────────────────────────────────────────────
// 2. PLAYLIST URL
// ─────────────────────────────────────────────

static PLAYLIST_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?xi)
        ^
        (?:
            # youtube.com/playlist?list=ID  or  /watch?...&list=ID
            https?://
            (?:\w+\.)?youtube\.com/
            (?:playlist|watch)\?
            (?:.*?[&;])??
            list=
            (?P<id_std>
                (?:
                    (?:PL|LL|EC|UU|FL|RD|UL|TL|PU|OLAK5uy_)[0-9A-Za-z_\-]{10,}
                    |
                    RDMM|WL|LL|LM
                )
            )
            (?:[\#&].*)?$
            |
            # youtu.be/VIDEO?list=ID
            https?://youtu\.be/
            [0-9A-Za-z_\-]{11}
            /?.*?\blist=
            (?P<id_ytbe>
                (?:
                    (?:PL|LL|EC|UU|FL|RD|UL|TL|PU|OLAK5uy_)[0-9A-Za-z_\-]{10,}
                    |
                    RDMM|WL|LL|LM
                )
            )
            (?:[\#&].*)?$
            |
            # Bare playlist ID (no domain). The inner ^ is redundant with the
            # outer ^ but kept for parity with the Python source.
            ^
            (?P<id_bare>
                (?:
                    (?:PL|LL|EC|UU|FL|RD|UL|TL|PU|OLAK5uy_)[0-9A-Za-z_\-]{10,}
                    |
                    RDMM|WL|LL|LM
                )
            )
            $
        )
        ",
    )
    .expect("invalid PLAYLIST_URL_RE")
});

/// Return the playlist ID if `url` is a YouTube playlist URL or bare ID, else `None`.
pub fn match_playlist_url(url: &str) -> Option<&str> {
    let caps = PLAYLIST_URL_RE.captures(url).ok().flatten()?;
    caps.name("id_std")
        .or_else(|| caps.name("id_ytbe"))
        .or_else(|| caps.name("id_bare"))
        .map(|m| m.as_str())
}

// ─────────────────────────────────────────────
// 3. CHANNEL URL
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    ChannelId,
    Custom,
    User,
    Handle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelMatch<'a> {
    pub kind: ChannelKind,
    pub id: &'a str,
}

// Kept for fidelity with the Python source. As in the original, this list is
// referenced only by a comment — the regex itself does not consult it. A URL
// like `youtube.com/c/trending` would still be classified as a custom channel.
#[allow(dead_code)]
const RESERVED_NAMES: &str = concat!(
    "channel|c|user|playlist|watch|w|v|embed|e|live|watch_popup|clip|",
    "shorts|movies|results|search|shared|hashtag|trending|explore|feed|feeds|",
    "browse|oembed|get_video_info|iframe_api|s/player|source|",
    "storefront|oops|index|account|t/terms|about|upload|signin|logout",
);

static CHANNEL_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?xi)
        ^
        https?://
        (?:\w+\.)?youtube\.com/
        (?:
            channel/(?P<channel_id>UC[0-9A-Za-z_\-]{22})
            |
            c/(?P<custom>[^/?\#&]+)
            |
            user/(?P<user>[^/?\#&]+)
            |
            # Handle (@handle)
            (?P<handle>@[\w.\-]+)
        )
        (?:/[^?\#]*)?(?:\?[^\#]*)?(?:\#.*)?
        $
        ",
    )
    .expect("invalid CHANNEL_URL_RE")
});

/// Return channel kind + id if `url` is a YouTube channel URL, else `None`.
pub fn match_channel_url<'a>(url: &'a str) -> Option<ChannelMatch<'a>> {
    let caps = CHANNEL_URL_RE.captures(url).ok().flatten()?;
    if let Some(m) = caps.name("channel_id") {
        return Some(ChannelMatch {
            kind: ChannelKind::ChannelId,
            id: m.as_str(),
        });
    }
    if let Some(m) = caps.name("custom") {
        return Some(ChannelMatch {
            kind: ChannelKind::Custom,
            id: m.as_str(),
        });
    }
    if let Some(m) = caps.name("user") {
        return Some(ChannelMatch {
            kind: ChannelKind::User,
            id: m.as_str(),
        });
    }
    if let Some(m) = caps.name("handle") {
        return Some(ChannelMatch {
            kind: ChannelKind::Handle,
            id: m.as_str(),
        });
    }
    None
}

pub fn video_url(id: &str) -> String {
    format!("https://youtu.be/{id}")
}

pub fn playlist_url(id: &str) -> String {
    format!("https://www.youtube.com/playlist?list={id}")
}

pub fn channel_url(kind: ChannelKind, id: &str) -> String {
    match kind {
        ChannelKind::ChannelId => format!("https://www.youtube.com/channel/{id}"),
        ChannelKind::Custom => format!("https://www.youtube.com/c/{id}"),
        ChannelKind::User => format!("https://www.youtube.com/user/{id}"),
        ChannelKind::Handle => format!("https://www.youtube.com/{id}"),
    }
}

pub fn canonicalize(source_key: &str, identifier: &str) -> Option<CanonicalizeResult> {
    // Only accept unresolved URLs; canonical source keys are passed straight to fetch_entry.
    if source_key != StandardProviderKeys::UNKNOWN_URL {
        return None;
    }
    if let Some(video_id) = match_video_url(identifier) {
        return Some(CanonicalizeResult {
            canonical_source_key: SOURCE.into(),
            canonical_identifier: video_url(video_id),
            entry_type: crate::providers::types::EntryType::Track,
            external_type: EXTERNAL_TYPE_VIDEO.into(),
        });
    }
    if let Some(playlist_id) = match_playlist_url(identifier) {
        return Some(CanonicalizeResult {
            canonical_source_key: SOURCE.into(),
            canonical_identifier: playlist_url(playlist_id),
            entry_type: crate::providers::types::EntryType::Release,
            external_type: EXTERNAL_TYPE_PLAYLIST.into(),
        });
    }
    if let Some(channel_match) = match_channel_url(identifier) {
        let external_type = match channel_match.kind {
            ChannelKind::ChannelId => EXTERNAL_TYPE_CHANNEL_ID,
            ChannelKind::Custom => EXTERNAL_TYPE_CUSTOM_CHANNEL,
            ChannelKind::User => EXTERNAL_TYPE_USER_CHANNEL,
            ChannelKind::Handle => EXTERNAL_TYPE_HANDLE,
        };
        return Some(CanonicalizeResult {
            canonical_source_key: SOURCE.into(),
            canonical_identifier: channel_url(channel_match.kind, channel_match.id),
            entry_type: crate::providers::types::EntryType::Artist,
            external_type: external_type.into(),
        });
    }
    None
}

// ─────────────────────────────────────────────
// Quick self-test (mirrors the Python __main__)
// ─────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_video_urls() {
        let video_tests: &[(&str, Option<&str>)] = &[
            (
                "https://www.youtube.com/watch?v=BaW_jenozKc",
                Some("BaW_jenozKc"),
            ),
            ("https://youtu.be/BaW_jenozKc", Some("BaW_jenozKc")),
            (
                "https://www.youtube.com/embed/BaW_jenozKc",
                Some("BaW_jenozKc"),
            ),
            (
                "https://www.youtube.com/shorts/BGQWPY4IigY",
                Some("BGQWPY4IigY"),
            ),
            (
                "https://www.youtube.com/live/qVv6vCqciTM",
                Some("qVv6vCqciTM"),
            ),
            (
                "https://music.youtube.com/watch?v=MgNrAu2pzNs",
                Some("MgNrAu2pzNs"),
            ),
            ("https://www.youtube.com/v/BaW_jenozKc", Some("BaW_jenozKc")),
            ("https://www.youtube.com/e/BaW_jenozKc", Some("BaW_jenozKc")),
            (
                "https://www.youtube.com/watch_popup?v=63RmMXCd_bQ",
                Some("63RmMXCd_bQ"),
            ),
            (
                "https://www.youtube.com/?v=BaW_jenozKc",
                Some("BaW_jenozKc"),
            ),
            (
                "https://www.youtube-nocookie.com/watch?v=BaW_jenozKc",
                Some("BaW_jenozKc"),
            ),
            (
                "https://youtubekids.com/watch?v=BaW_jenozKc",
                Some("BaW_jenozKc"),
            ),
            (
                "https://www.youtube.com/watch?v=BaW_jenozKc&list=PL...",
                Some("BaW_jenozKc"),
            ),
            (
                "https://www.youtube.com/watch?feature=player_embedded&v=BaW_jenozKc",
                Some("BaW_jenozKc"),
            ),
            // should NOT match
            (
                "https://www.youtube.com/channel/UCabc123def456ghi789jkl0",
                None,
            ),
            ("https://www.youtube.com/playlist?list=PLABC123", None),
        ];

        for (url, expected) in video_tests {
            let got = match_video_url(url);
            assert_eq!(
                got, *expected,
                "match_video_url({:?}) => {:?} (expected {:?})",
                url, got, expected
            );
        }
    }

    #[test]
    fn test_playlist_urls() {
        let playlist_tests: &[(&str, Option<&str>)] = &[
            (
                "https://www.youtube.com/playlist?list=PLBB231211A4F62143",
                Some("PLBB231211A4F62143"),
            ),
            (
                "https://www.youtube.com/watch?v=BaW_jenozKc&list=PLBB231211A4F62143",
                Some("PLBB231211A4F62143"),
            ),
            (
                "https://youtu.be/yeWKywCrFtk?list=PL2qgrgXsNUG5ig9cat4ohreBjYLAPC0J5",
                Some("PL2qgrgXsNUG5ig9cat4ohreBjYLAPC0J5"),
            ),
            ("PLBB231211A4F62143", Some("PLBB231211A4F62143")),
            ("WL", Some("WL")),
            ("RDMM", Some("RDMM")),
            // should NOT match
            ("https://www.youtube.com/watch?v=BaW_jenozKc", None),
            ("https://www.youtube.com/channel/UCabc123", None),
        ];

        for (url, expected) in playlist_tests {
            let got = match_playlist_url(url);
            assert_eq!(
                got, *expected,
                "match_playlist_url({:?}) => {:?} (expected {:?})",
                url, got, expected
            );
        }
    }

    #[test]
    fn test_channel_urls() {
        let channel_tests: &[(&str, Option<(ChannelKind, &str)>)] = &[
            (
                "https://www.youtube.com/channel/UCXuqSBlHAE6Xw-yeJA0Tunw",
                Some((ChannelKind::ChannelId, "UCXuqSBlHAE6Xw-yeJA0Tunw")),
            ),
            (
                "https://www.youtube.com/c/3blue1brown",
                Some((ChannelKind::Custom, "3blue1brown")),
            ),
            (
                "https://www.youtube.com/user/NASAgovVideo",
                Some((ChannelKind::User, "NASAgovVideo")),
            ),
            (
                "https://www.youtube.com/@kurzgesagt",
                Some((ChannelKind::Handle, "@kurzgesagt")),
            ),
            (
                "https://www.youtube.com/@kurzgesagt/videos",
                Some((ChannelKind::Handle, "@kurzgesagt")),
            ),
            // should NOT match
            ("https://www.youtube.com/watch?v=BaW_jenozKc", None),
            ("https://www.youtube.com/playlist?list=PLABC123", None),
            ("https://www.youtube.com/trending", None),
        ];

        for (url, expected) in channel_tests {
            let got = match_channel_url(url).map(|c| (c.kind, c.id));
            assert_eq!(
                got, *expected,
                "match_channel_url({:?}) => {:?} (expected {:?})",
                url, got, expected
            );
        }
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
