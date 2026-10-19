pub mod canonicalize;
mod channel;
mod client;
pub mod coalesce;
pub mod matcher;
mod playlist;
mod types;
mod video;

pub use canonicalize::{ChannelKind, match_channel_url, match_playlist_url, match_video_url};
pub use channel::get_channel_raw;
pub use client::YoutubeClient;
pub use coalesce::YouTubeListRule;
pub use playlist::{get_playlist_items_raw, get_playlist_raw};
pub use video::get_video_raw;

use crate::providers::std_values::StandardProviderKeys;
pub use types::Provider;

pub const SOURCE: &str = StandardProviderKeys::YOUTUBE;

/// The variant worth storing (`Image::pick`) of a YouTube `snippet`'s
/// `thumbnails` map (`default`/`medium`/`high`/`standard`/`maxres`), which
/// every snippet type here keeps in its flattened `extra`.
pub(crate) fn snippet_thumbnail(
    snippet_extra: &crate::providers::backends::ExtraJSON,
) -> Option<crate::providers::types::Image> {
    let thumbnails = snippet_extra.get("thumbnails")?.as_object()?;
    crate::providers::types::Image::pick(thumbnails.values().filter_map(|t| {
        let size = |k: &str| {
            t.get(k)
                .and_then(serde_json::Value::as_u64)
                .map(|n| n as u32)
        };
        Some(crate::providers::types::Image::new(
            t.get("url")?.as_str()?,
            size("width"),
            size("height"),
        ))
    }))
}

#[cfg(test)]
mod thumbnail_tests {
    #[test]
    fn picks_medium_thumbnail() {
        let extra = serde_json::json!({ "thumbnails": {
            "default": { "url": "d", "width": 120, "height": 90 },
            "medium": { "url": "m", "width": 320, "height": 180 },
            "high": { "url": "h", "width": 480, "height": 360 },
        }});
        let image = super::snippet_thumbnail(extra.as_object().unwrap()).unwrap();
        assert_eq!(image.url, "m");
    }
}
