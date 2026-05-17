use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// --- External type constants ---

pub const EXTERNAL_TYPE_TRACK: &str = "spotify:track";
pub const EXTERNAL_TYPE_ALBUM: &str = "spotify:album";
pub const EXTERNAL_TYPE_PLAYLIST: &str = "spotify:playlist";
pub const EXTERNAL_TYPE_ARTIST: &str = "spotify:artist";

// --- Shared API response types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimpleArtist {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub external_urls: ExternalUrls,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExternalUrls {
    pub spotify: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Image {
    pub url: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Generic Spotify paging object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Paging<T> {
    pub items: Vec<T>,
    pub next: Option<String>,
    pub offset: u32,
    pub limit: u32,
    pub total: u32,
}
