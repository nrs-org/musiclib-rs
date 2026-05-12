use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use std::sync::Arc;

// dict of external identifier (mostly URLs), grouped by source (e.g. "wikidata", "spotify", etc.)
#[derive(Debug, Clone, Default)]
pub struct ExternalSources(pub HashMap<Cow<'static, str>, HashSet<String>>);

impl ExternalSources {
    pub fn get(&self, source: &str) -> Option<&HashSet<String>> {
        self.0.get(source)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the first identifier found across all sources, if any.
    pub fn first_identifier(&self) -> Option<&str> {
        self.0
            .values()
            .flat_map(|s| s.iter())
            .next()
            .map(|s| s.as_str())
    }
}

impl<T> From<T> for ExternalSources
where
    HashMap<Cow<'static, str>, HashSet<String>>: From<T>,
{
    fn from(sources: T) -> Self {
        ExternalSources(HashMap::from(sources))
    }
}

// track position within a release, with optional disc number (for multi-disc releases)
pub struct TrackPosition {
    pub disc_no: Option<i32>,
    pub track_no: i32,
}

// specific data for different entry types
pub enum EntrySpecificData {
    Track {
        duration_ms: Option<i64>,
        positions: HashMap<String, TrackPosition>,
    },
    Release {
        release_type: Option<String>,
        num_discs: Option<i32>,
        num_tracks: Option<i32>,
    },
    ReleaseGroup {
        primary_type: Option<String>,
    },
    Artist,
}

// type of entry (artist, release group, release, track)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum EntryType {
    Artist,
    ReleaseGroup,
    Release,
    #[default]
    Track,
}

// entry alias, with optional locale and extra metadata
#[derive(Default)]
pub struct Alias {
    pub name: String,
    pub source: String,
    pub locale: Option<String>,
    pub extra: serde_json::Value,
    // treat this with a grain of salt, an entry will have many "primary" aliases (each provider has at least one).
    pub primary: bool,
}

// reference to a child entry (e.g. track within a release), with optional name and position
#[derive(Default)]
pub struct ChildRef {
    pub entry_type: EntryType,
    pub sources: ExternalSources,
    pub name: Option<String>,
    pub position: Option<TrackPosition>,
    pub contributions: Vec<Contribution>,
}

// contribution of an artist to an entry, with role (e.g. "main", "featured", "producer", etc.) and
// optional flag for main artist
#[derive(Default)]
pub struct Contribution {
    pub role: String,
    pub main_artist: bool,
    pub extra: serde_json::Value,
    pub source: Cow<'static, str>,
}

// result of fetching an entry, with optional release date, external sources, extra metadata,
// specific data for the entry type, child entries (e.g. tracks within a release), contributions
// (e.g. artists on a track), and aliases
pub struct EntityResult {
    pub release_date: Option<OffsetDateTime>,
    pub sources: ExternalSources,
    pub extra: serde_json::Value,
    pub specific_data: EntrySpecificData,
    pub children: Vec<ChildRef>,
    pub aliases: Vec<Alias>,
}

// result of canonicalizing an identifier, with canonical identifier, entry type, and external type
pub struct CanonicalizeResult {
    pub canonical_identifier: String,
    pub entry_type: EntryType,
    pub external_type: Cow<'static, str>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Invalid URL: {0}")]
    InvalidUrl(String),
    #[error("HTTP error: {0}")]
    Http(#[from] crate::http::Error),
    #[error("Invalid credentials: {0}")]
    InvalidCredentials(String),
    #[error("Missing credentials: {0}")]
    MissingCredentials(String),
}

// Matchers based on entry metadata (requires fetching the entry)
#[derive(Clone, Serialize, Deserialize)]
pub enum EntryDataMatcher {
    // Generic — available for all backends
    EntryType(EntryType),
    NameRegex(String),
    DurationRange { min: Option<u64>, max: Option<u64> },
    HasSource(String),

    // Backend-specific
    YouTube(YouTubeDataMatcher),
    // MusicBrainz(MusicBrainzDataMatcher),
    // Spotify(SpotifyDataMatcher),
}

// YouTube-specific entry data matchers
#[derive(Clone, Serialize, Deserialize)]
pub enum YouTubeDataMatcher {
    DescriptionRegex(String),
    CategoryId(String),
}

// Matchers based on parent-child relationship (no fetch needed)
#[derive(Clone, Serialize, Deserialize)]
pub enum RelationMatcher {
    IndexRange { min: Option<u32>, max: Option<u32> },
}

// Matchers based on children (requires fetching children)
#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum QuantifierMode {
    Count { min: Option<u32>, max: Option<u32> },
    Ratio { min: Option<f64>, max: Option<f64> },
}

#[derive(Clone, Serialize, Deserialize)]
pub enum ChildMatcher {
    Always,
    EntryData(EntryDataMatcher),
    Relation(RelationMatcher),
    ChildrenSatisfy {
        matcher: Box<ChildMatcherExpr>,
        mode: QuantifierMode,
    },
}

#[derive(Clone, Serialize, Deserialize)]
pub enum ChildMatcherExpr {
    Matcher(ChildMatcher),
    Not(Box<ChildMatcherExpr>),
    All(Vec<ChildMatcherExpr>),
    Any(Vec<ChildMatcherExpr>),
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ChildRule {
    pub matcher: ChildMatcherExpr,
    pub options: Arc<EntryFetchOptions>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct EntryFetchOptions {
    pub child_rules: Vec<ChildRule>,
}
