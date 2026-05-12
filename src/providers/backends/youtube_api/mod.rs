mod canonicalize;
mod channel;
mod client;
mod playlist;
mod types;
mod video;

pub use canonicalize::{ChannelKind, match_channel_url, match_playlist_url, match_video_url};
pub use channel::get_channel_raw;
pub use client::YoutubeClient;
pub use playlist::{get_playlist_items_raw, get_playlist_raw};
pub use types::EntryFetchOptions;
pub use video::get_video_raw;

use crate::providers::std_values::StandardProviderKeys;

pub const SOURCE: &str = StandardProviderKeys::YOUTUBE;
