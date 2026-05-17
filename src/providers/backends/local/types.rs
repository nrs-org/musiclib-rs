use std::collections::HashMap;

use serde::Deserialize;

use crate::providers::types::EntryType;

/// Top-level structure of a local YAML entry file.
#[derive(Debug, Deserialize)]
pub struct LocalEntry {
    #[serde(rename = "type")]
    pub entry_type: EntryType,
    #[serde(default)]
    pub aliases: Vec<LocalAlias>,
    pub release_date: Option<String>,
    /// External sources: map of source name → list of identifiers/URLs.
    #[serde(default)]
    pub sources: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub children: Vec<LocalChild>,

    // Track-specific
    pub duration_ms: Option<i64>,

    // Release-specific
    pub release_type: Option<String>,
    pub num_discs: Option<i32>,
    pub num_tracks: Option<i32>,

    // ReleaseGroup-specific
    pub primary_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LocalAlias {
    pub name: String,
    #[serde(default)]
    pub primary: bool,
    pub locale: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LocalChild {
    #[serde(rename = "type")]
    pub entry_type: EntryType,
    pub name: Option<String>,
    /// External sources for this child (e.g. `local: ["./track.yaml"]`).
    #[serde(default)]
    pub sources: HashMap<String, Vec<String>>,
    pub position: Option<LocalTrackPosition>,
    #[serde(default)]
    pub contributions: Vec<LocalContribution>,
}

#[derive(Debug, Deserialize)]
pub struct LocalTrackPosition {
    pub track_no: i32,
    pub disc_no: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct LocalContribution {
    pub role: String,
    #[serde(default)]
    pub main_artist: bool,
}
