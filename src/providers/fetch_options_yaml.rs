//! Human-readable YAML serialization for [`EntryFetchOptions`].
//!
//! The internal representation uses an [`EntryFetchOptionsPool`] (flat arena of options nodes
//! referenced by integer id) which is efficient but not human-friendly. This module provides
//! an intermediate tree representation — [`YamlFetchDocument`] — where child options are either
//! **inlined** or **named** (referenced by string key). The document is a flat map of named
//! entries; the reserved key `main` is the root entry point. All other keys are reusable named
//! sets that may reference each other or themselves, enabling shared and recursive fetch rules
//! without memory-leak cycles.
//!
//! # YAML shape
//!
//! ```yaml
//! # Named, reusable option sets (optional)
//! mv_only:
//!   child_rules:
//!     - match: { name_regex: "【MV】" }
//!       fetch: mv_only        # self-reference → recurse indefinitely
//!
//! # Root entry point (reserved key)
//! main:
//!   child_rules:
//!     - match: { entry_type: release }
//!       fetch: mv_only        # reference to named set
//!     - match: always         # fetch omitted → use default (no child rules)
//! ```
//!
//! ## Matcher expressions
//!
//! Unit matchers are bare strings; all others are single-key maps:
//!
//! ```yaml
//! match: always
//! match: { entry_type: track }        # track | release | release_group | artist
//! match: { name_regex: "pattern" }
//! match: { has_source: spotify }
//! match: { duration_range: { min: 60000, max: 300000 } }  # ms; both optional
//! match: { index_range: { min: 0, max: 50 } }             # both optional
//! match: { youtube: { description_regex: "pattern" } }
//! match: { youtube: { category_id: "10" } }
//! match:
//!   children_satisfy:
//!     matcher: { name_regex: "MV" }
//!     mode:
//!       ratio: { min: 0.75 }    # or: count: { min: 1, max: 100 }
//! match: { not: <matcher-expr> }
//! match:
//!   all:
//!     - { entry_type: track }
//!     - { name_regex: "MV" }
//! match:
//!   any:
//!     - { entry_type: track }
//!     - { entry_type: release }
//! ```

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::providers::types::{
    ChildMatcher, ChildMatcherExpr, ChildRule, EntryDataMatcher, EntryFetchOptions,
    EntryFetchOptionsPool, EntryType, MusicBrainzDataMatcher, OptionsId, QuantifierMode,
    RelationMatcher, YouTubeDataMatcher,
};

// ---------------------------------------------------------------------------
// YAML intermediate types
// ---------------------------------------------------------------------------

/// Top-level document: a flat map of named option sets.
///
/// The reserved key `"main"` is the root entry point. All other keys are
/// reusable named sets that may be referenced via `fetch: <name>`. Referencing
/// `"main"` directly in a `fetch:` field is an error.
#[derive(Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct YamlFetchDocument(pub HashMap<String, YamlEntryFetchOptions>);

/// An inline options block.
#[derive(Serialize, Deserialize, Default)]
pub struct YamlEntryFetchOptions {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub child_rules: Vec<YamlChildRule>,
}

/// One rule: a matcher and the options to use when fetching a matched child.
#[derive(Serialize, Deserialize)]
pub struct YamlChildRule {
    #[serde(rename = "match")]
    pub matcher: YamlMatcherExpr,

    /// How to fetch matched children.
    /// - Omitted → use default (no child rules).
    /// - `"name"` → reference a named entry in `options:`.
    /// - Inline block → anonymous options (no sharing / recursion).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch: Option<YamlFetchRef>,
}

/// Either a string reference to a named option set, or an anonymous inline block.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum YamlFetchRef {
    Ref(String),
    Inline(YamlEntryFetchOptions),
}

// ---------------------------------------------------------------------------
// YamlMatcherExpr — custom serde
//
// serde_yaml_ng does not support serde's external-tag convention for maps
// (it would require YAML native !tags). We implement custom Serialize /
// Deserialize that emit and parse bare strings ("always") and single-key maps
// ({entry_type: track}) directly.
// ---------------------------------------------------------------------------

pub enum YamlMatcherExpr {
    Always,
    EntryType(EntryType),
    ExternalType(String),
    NameRegex(String),
    HasSource(String),
    AppearsOn(bool),
    DurationRange { min: Option<u64>, max: Option<u64> },
    IndexRange { min: Option<u32>, max: Option<u32> },
    YouTube(YamlYouTubeMatcher),
    MusicBrainz(YamlMusicBrainzMatcher),
    ChildrenSatisfy(YamlChildrenSatisfy),
    Not(Box<YamlMatcherExpr>),
    All(Vec<YamlMatcherExpr>),
    Any(Vec<YamlMatcherExpr>),
}

/// Helper for `{min, max}` range fields (u64).
#[derive(Serialize, Deserialize, Default)]
struct RangeU64 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    min: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max: Option<u64>,
}

/// Helper for `{min, max}` range fields (u32).
#[derive(Serialize, Deserialize, Default)]
struct RangeU32 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max: Option<u32>,
}

/// Helper for `{min, max}` range fields (f64).
#[derive(Serialize, Deserialize, Default)]
struct RangeF64 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max: Option<f64>,
}

fn serialize_single_map<S, V>(s: S, key: &str, value: &V) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    V: Serialize,
{
    let mut map = s.serialize_map(Some(1))?;
    map.serialize_entry(key, value)?;
    map.end()
}

impl Serialize for YamlMatcherExpr {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Always => s.serialize_str("always"),
            Self::EntryType(t) => serialize_single_map(s, "entry_type", t),
            Self::ExternalType(t) => serialize_single_map(s, "external_type", t),
            Self::NameRegex(p) => serialize_single_map(s, "name_regex", p),
            Self::HasSource(src) => serialize_single_map(s, "has_source", src),
            Self::AppearsOn(b) => serialize_single_map(s, "appears_on", b),
            Self::DurationRange { min, max } => serialize_single_map(
                s,
                "duration_range",
                &RangeU64 {
                    min: *min,
                    max: *max,
                },
            ),
            Self::IndexRange { min, max } => serialize_single_map(
                s,
                "index_range",
                &RangeU32 {
                    min: *min,
                    max: *max,
                },
            ),
            Self::YouTube(yt) => serialize_single_map(s, "youtube", yt),
            Self::MusicBrainz(mb) => serialize_single_map(s, "musicbrainz", mb),
            Self::ChildrenSatisfy(cs) => serialize_single_map(s, "children_satisfy", cs),
            Self::Not(inner) => serialize_single_map(s, "not", inner),
            Self::All(exprs) => serialize_single_map(s, "all", exprs),
            Self::Any(exprs) => serialize_single_map(s, "any", exprs),
        }
    }
}

impl<'de> Deserialize<'de> for YamlMatcherExpr {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct MatcherVisitor;

        impl<'de> Visitor<'de> for MatcherVisitor {
            type Value = YamlMatcherExpr;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, r#""always" or a single-key matcher map"#)
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                match v {
                    "always" => Ok(YamlMatcherExpr::Always),
                    other => Err(E::unknown_variant(other, &["always"])),
                }
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let key: String = map
                    .next_key()?
                    .ok_or_else(|| de::Error::custom("expected a matcher key, got empty map"))?;

                let expr = match key.as_str() {
                    "entry_type" => YamlMatcherExpr::EntryType(map.next_value()?),
                    "external_type" => YamlMatcherExpr::ExternalType(map.next_value()?),
                    "name_regex" => YamlMatcherExpr::NameRegex(map.next_value()?),
                    "has_source" => YamlMatcherExpr::HasSource(map.next_value()?),
                    "appears_on" => YamlMatcherExpr::AppearsOn(map.next_value()?),
                    "duration_range" => {
                        let r: RangeU64 = map.next_value()?;
                        YamlMatcherExpr::DurationRange {
                            min: r.min,
                            max: r.max,
                        }
                    }
                    "index_range" => {
                        let r: RangeU32 = map.next_value()?;
                        YamlMatcherExpr::IndexRange {
                            min: r.min,
                            max: r.max,
                        }
                    }
                    "youtube" => YamlMatcherExpr::YouTube(map.next_value()?),
                    "musicbrainz" => YamlMatcherExpr::MusicBrainz(map.next_value()?),
                    "children_satisfy" => YamlMatcherExpr::ChildrenSatisfy(map.next_value()?),
                    "not" => YamlMatcherExpr::Not(map.next_value()?),
                    "all" => YamlMatcherExpr::All(map.next_value()?),
                    "any" => YamlMatcherExpr::Any(map.next_value()?),
                    other => {
                        return Err(de::Error::unknown_field(
                            other,
                            &[
                                "entry_type",
                                "external_type",
                                "name_regex",
                                "has_source",
                                "appears_on",
                                "duration_range",
                                "index_range",
                                "youtube",
                                "musicbrainz",
                                "children_satisfy",
                                "not",
                                "all",
                                "any",
                            ],
                        ));
                    }
                };

                // Consume any remaining keys (should be none in a well-formed document)
                while map.next_key::<String>()?.is_some() {
                    map.next_value::<de::IgnoredAny>()?;
                }

                Ok(expr)
            }
        }

        d.deserialize_any(MatcherVisitor)
    }
}

// ---------------------------------------------------------------------------
// YamlYouTubeMatcher — custom serde (same reason: external-tag + map)
// ---------------------------------------------------------------------------

pub enum YamlYouTubeMatcher {
    DescriptionRegex(String),
    CategoryId(String),
}

impl Serialize for YamlYouTubeMatcher {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::DescriptionRegex(p) => serialize_single_map(s, "description_regex", p),
            Self::CategoryId(id) => serialize_single_map(s, "category_id", id),
        }
    }
}

impl<'de> Deserialize<'de> for YamlYouTubeMatcher {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct YouTubeVisitor;

        impl<'de> Visitor<'de> for YouTubeVisitor {
            type Value = YamlYouTubeMatcher;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "a YouTube matcher map")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let key: String = map
                    .next_key()?
                    .ok_or_else(|| de::Error::custom("expected a YouTube matcher key"))?;
                let m = match key.as_str() {
                    "description_regex" => YamlYouTubeMatcher::DescriptionRegex(map.next_value()?),
                    "category_id" => YamlYouTubeMatcher::CategoryId(map.next_value()?),
                    other => {
                        return Err(de::Error::unknown_field(
                            other,
                            &["description_regex", "category_id"],
                        ));
                    }
                };
                while map.next_key::<String>()?.is_some() {
                    map.next_value::<de::IgnoredAny>()?;
                }
                Ok(m)
            }
        }

        d.deserialize_map(YouTubeVisitor)
    }
}

// ---------------------------------------------------------------------------
// YamlMusicBrainzMatcher — custom serde
// ---------------------------------------------------------------------------

pub enum YamlMusicBrainzMatcher {
    ReleaseGroupPrimaryType(String),
    ReleaseGroupHasSecondaryType(String),
    ReleaseStatus(String),
    ReleaseCountry(String),
    RecordingIsVideo,
}

impl Serialize for YamlMusicBrainzMatcher {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::ReleaseGroupPrimaryType(t) => {
                serialize_single_map(s, "release_group_primary_type", t)
            }
            Self::ReleaseGroupHasSecondaryType(t) => {
                serialize_single_map(s, "release_group_has_secondary_type", t)
            }
            Self::ReleaseStatus(st) => serialize_single_map(s, "release_status", st),
            Self::ReleaseCountry(c) => serialize_single_map(s, "release_country", c),
            Self::RecordingIsVideo => s.serialize_str("recording_is_video"),
        }
    }
}

impl<'de> Deserialize<'de> for YamlMusicBrainzMatcher {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct MbVisitor;

        impl<'de> Visitor<'de> for MbVisitor {
            type Value = YamlMusicBrainzMatcher;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "a MusicBrainz matcher string or map")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                match v {
                    "recording_is_video" => Ok(YamlMusicBrainzMatcher::RecordingIsVideo),
                    other => Err(E::unknown_variant(other, &["recording_is_video"])),
                }
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let key: String = map
                    .next_key()?
                    .ok_or_else(|| de::Error::custom("expected a MusicBrainz matcher key"))?;
                let m = match key.as_str() {
                    "release_group_primary_type" => {
                        YamlMusicBrainzMatcher::ReleaseGroupPrimaryType(map.next_value()?)
                    }
                    "release_group_has_secondary_type" => {
                        YamlMusicBrainzMatcher::ReleaseGroupHasSecondaryType(map.next_value()?)
                    }
                    "release_status" => YamlMusicBrainzMatcher::ReleaseStatus(map.next_value()?),
                    "release_country" => YamlMusicBrainzMatcher::ReleaseCountry(map.next_value()?),
                    other => {
                        return Err(de::Error::unknown_field(
                            other,
                            &[
                                "release_group_primary_type",
                                "release_group_has_secondary_type",
                                "release_status",
                                "release_country",
                            ],
                        ));
                    }
                };
                while map.next_key::<String>()?.is_some() {
                    map.next_value::<de::IgnoredAny>()?;
                }
                Ok(m)
            }
        }

        d.deserialize_any(MbVisitor)
    }
}

// ---------------------------------------------------------------------------
// YamlQuantifierMode — custom serde
// ---------------------------------------------------------------------------

pub enum YamlQuantifierMode {
    Ratio { min: Option<f64>, max: Option<f64> },
    Count { min: Option<u32>, max: Option<u32> },
}

impl Serialize for YamlQuantifierMode {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Ratio { min, max } => serialize_single_map(
                s,
                "ratio",
                &RangeF64 {
                    min: *min,
                    max: *max,
                },
            ),
            Self::Count { min, max } => serialize_single_map(
                s,
                "count",
                &RangeU32 {
                    min: *min,
                    max: *max,
                },
            ),
        }
    }
}

impl<'de> Deserialize<'de> for YamlQuantifierMode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct ModeVisitor;

        impl<'de> Visitor<'de> for ModeVisitor {
            type Value = YamlQuantifierMode;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "a quantifier mode map (ratio or count)")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let key: String = map
                    .next_key()?
                    .ok_or_else(|| de::Error::custom("expected a quantifier mode key"))?;
                let m = match key.as_str() {
                    "ratio" => {
                        let r: RangeF64 = map.next_value()?;
                        YamlQuantifierMode::Ratio {
                            min: r.min,
                            max: r.max,
                        }
                    }
                    "count" => {
                        let r: RangeU32 = map.next_value()?;
                        YamlQuantifierMode::Count {
                            min: r.min,
                            max: r.max,
                        }
                    }
                    other => return Err(de::Error::unknown_field(other, &["ratio", "count"])),
                };
                while map.next_key::<String>()?.is_some() {
                    map.next_value::<de::IgnoredAny>()?;
                }
                Ok(m)
            }
        }

        d.deserialize_map(ModeVisitor)
    }
}

// ---------------------------------------------------------------------------
// YamlChildrenSatisfy — derives are fine (its fields have custom serde above)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
pub struct YamlChildrenSatisfy {
    pub matcher: Box<YamlMatcherExpr>,
    pub mode: YamlQuantifierMode,
}

// ---------------------------------------------------------------------------
// Conversion error
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum YamlConversionError {
    #[error("unknown option reference: \"{0}\"")]
    UnknownRef(String),
    #[error("I/O error reading \"{path}\": {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("YAML parse error in \"{path}\": {source}")]
    YamlParse {
        path: PathBuf,
        #[source]
        source: serde_yaml_ng::Error,
    },
    #[error("cross-file ref \"{0}\" used in in-memory context (no file path available)")]
    CrossFileRefInMemory(String),
}

// ---------------------------------------------------------------------------
// YamlFetchDocument → (Arc<EntryFetchOptionsPool>, OptionsId)
// ---------------------------------------------------------------------------

impl YamlFetchDocument {
    /// Convert into a pool + root id.
    ///
    /// Two-pass algorithm:
    /// 1. Pre-allocate a placeholder slot for every entry (including `"main"`) so that forward
    ///    and cyclic references resolve to a valid id before the content is known.
    /// 2. Fill each slot with the real content.
    pub fn into_pool(self) -> Result<(Arc<EntryFetchOptionsPool>, OptionsId), YamlConversionError> {
        let mut pool = EntryFetchOptionsPool::default();
        let mut name_to_id: HashMap<String, OptionsId> = HashMap::new();

        // Pass 1 — reserve ids for all entries
        for name in self.0.keys() {
            let id = pool.insert(EntryFetchOptions::default());
            name_to_id.insert(name.clone(), id);
        }

        // Pass 2 — resolve and patch each entry
        for (name, yaml_opts) in &self.0 {
            let id = name_to_id[name];
            let opts = resolve_options(yaml_opts, &name_to_id, &mut pool)?;
            pool.patch(id, opts);
        }

        let root_id = name_to_id
            .get("main")
            .copied()
            .unwrap_or(EntryFetchOptionsPool::DEFAULT_ID);

        Ok((Arc::new(pool), root_id))
    }
}

fn resolve_options(
    yaml: &YamlEntryFetchOptions,
    name_to_id: &HashMap<String, OptionsId>,
    pool: &mut EntryFetchOptionsPool,
) -> Result<EntryFetchOptions, YamlConversionError> {
    let child_rules = yaml
        .child_rules
        .iter()
        .map(|r| {
            let options_id = resolve_fetch_ref(&r.fetch, name_to_id, pool)?;
            Ok(ChildRule {
                matcher: convert_matcher_expr(&r.matcher)?,
                options_id,
            })
        })
        .collect::<Result<Vec<_>, YamlConversionError>>()?;

    Ok(EntryFetchOptions { child_rules })
}

fn resolve_fetch_ref(
    fetch: &Option<YamlFetchRef>,
    name_to_id: &HashMap<String, OptionsId>,
    pool: &mut EntryFetchOptionsPool,
) -> Result<Option<OptionsId>, YamlConversionError> {
    match fetch {
        None => Ok(None),
        Some(YamlFetchRef::Ref(name)) => {
            if name.contains("::") {
                return Err(YamlConversionError::CrossFileRefInMemory(name.clone()));
            }
            name_to_id
                .get(name)
                .copied()
                .map(Some)
                .ok_or_else(|| YamlConversionError::UnknownRef(name.clone()))
        }
        Some(YamlFetchRef::Inline(inline)) => {
            let opts = resolve_options(inline, name_to_id, pool)?;
            Ok(Some(pool.insert(opts)))
        }
    }
}

fn convert_matcher_expr(expr: &YamlMatcherExpr) -> Result<ChildMatcherExpr, YamlConversionError> {
    Ok(match expr {
        YamlMatcherExpr::Always => ChildMatcherExpr::Matcher(ChildMatcher::Always),

        YamlMatcherExpr::EntryType(t) => {
            ChildMatcherExpr::Matcher(ChildMatcher::EntryData(EntryDataMatcher::EntryType(*t)))
        }

        YamlMatcherExpr::ExternalType(t) => ChildMatcherExpr::Matcher(ChildMatcher::EntryData(
            EntryDataMatcher::ExternalType(t.clone()),
        )),

        YamlMatcherExpr::NameRegex(p) => ChildMatcherExpr::Matcher(ChildMatcher::EntryData(
            EntryDataMatcher::NameRegex(p.clone()),
        )),

        YamlMatcherExpr::AppearsOn(b) => {
            ChildMatcherExpr::Matcher(ChildMatcher::EntryData(EntryDataMatcher::AppearsOn(*b)))
        }
        YamlMatcherExpr::HasSource(s) => ChildMatcherExpr::Matcher(ChildMatcher::EntryData(
            EntryDataMatcher::HasSource(s.clone()),
        )),

        YamlMatcherExpr::DurationRange { min, max } => {
            ChildMatcherExpr::Matcher(ChildMatcher::EntryData(EntryDataMatcher::DurationRange {
                min: *min,
                max: *max,
            }))
        }

        YamlMatcherExpr::IndexRange { min, max } => {
            ChildMatcherExpr::Matcher(ChildMatcher::Relation(RelationMatcher::IndexRange {
                min: *min,
                max: *max,
            }))
        }

        YamlMatcherExpr::YouTube(yt) => ChildMatcherExpr::Matcher(ChildMatcher::EntryData(
            EntryDataMatcher::YouTube(match yt {
                YamlYouTubeMatcher::DescriptionRegex(p) => {
                    YouTubeDataMatcher::DescriptionRegex(p.clone())
                }
                YamlYouTubeMatcher::CategoryId(id) => YouTubeDataMatcher::CategoryId(id.clone()),
            }),
        )),

        YamlMatcherExpr::MusicBrainz(mb) => ChildMatcherExpr::Matcher(ChildMatcher::EntryData(
            EntryDataMatcher::MusicBrainz(match mb {
                YamlMusicBrainzMatcher::ReleaseGroupPrimaryType(t) => {
                    MusicBrainzDataMatcher::ReleaseGroupPrimaryType(t.clone())
                }
                YamlMusicBrainzMatcher::ReleaseGroupHasSecondaryType(t) => {
                    MusicBrainzDataMatcher::ReleaseGroupHasSecondaryType(t.clone())
                }
                YamlMusicBrainzMatcher::ReleaseStatus(s) => {
                    MusicBrainzDataMatcher::ReleaseStatus(s.clone())
                }
                YamlMusicBrainzMatcher::ReleaseCountry(c) => {
                    MusicBrainzDataMatcher::ReleaseCountry(c.clone())
                }
                YamlMusicBrainzMatcher::RecordingIsVideo => {
                    MusicBrainzDataMatcher::RecordingIsVideo
                }
            }),
        )),

        YamlMatcherExpr::ChildrenSatisfy(cs) => {
            ChildMatcherExpr::Matcher(ChildMatcher::ChildrenSatisfy {
                matcher: Box::new(convert_matcher_expr(&cs.matcher)?),
                mode: match cs.mode {
                    YamlQuantifierMode::Ratio { min, max } => QuantifierMode::Ratio { min, max },
                    YamlQuantifierMode::Count { min, max } => QuantifierMode::Count { min, max },
                },
            })
        }

        YamlMatcherExpr::Not(inner) => {
            ChildMatcherExpr::Not(Box::new(convert_matcher_expr(inner)?))
        }

        YamlMatcherExpr::All(exprs) => ChildMatcherExpr::All(
            exprs
                .iter()
                .map(convert_matcher_expr)
                .collect::<Result<_, _>>()?,
        ),

        YamlMatcherExpr::Any(exprs) => ChildMatcherExpr::Any(
            exprs
                .iter()
                .map(convert_matcher_expr)
                .collect::<Result<_, _>>()?,
        ),
    })
}

// ---------------------------------------------------------------------------
// (EntryFetchOptionsPool, OptionsId) → YamlFetchDocument
// ---------------------------------------------------------------------------

impl YamlFetchDocument {
    /// Convert a pool + root id back into a YAML document.
    ///
    /// Entries referenced more than once, or that participate in a reference cycle
    /// (including self-references), are emitted as named entries in `options:`.
    /// Singly-referenced, acyclic entries are inlined at their point of use.
    /// `DEFAULT_ID` (id 0, always empty rules) is never emitted — rules that use it
    /// simply omit `fetch:`.
    pub fn from_pool(pool: &EntryFetchOptionsPool, root_id: OptionsId) -> Self {
        // Count in-degrees and detect cycles via DFS from root
        let mut in_degree: HashMap<OptionsId, usize> = HashMap::new();
        let mut stack_set: HashSet<OptionsId> = HashSet::new();
        let mut cyclic: HashSet<OptionsId> = HashSet::new();

        collect_refs(pool, root_id, &mut in_degree, &mut stack_set, &mut cyclic);

        // Entries that must be named: referenced > once OR cyclic
        // DEFAULT_ID is excluded (it's the implicit "no rules" sentinel)
        let must_name: HashSet<OptionsId> = in_degree
            .iter()
            .filter(|&(&id, &count)| {
                id != EntryFetchOptionsPool::DEFAULT_ID && (count > 1 || cyclic.contains(&id))
            })
            .map(|(&id, _)| id)
            .collect();

        // Assign stable deterministic names (sorted by id)
        let mut named_ids: Vec<OptionsId> = must_name.iter().copied().collect();
        named_ids.sort_unstable();
        let names: HashMap<OptionsId, String> = named_ids
            .iter()
            .enumerate()
            .map(|(i, &id)| (id, format!("opts_{i}")))
            .collect();

        let mut map: HashMap<String, YamlEntryFetchOptions> = names
            .iter()
            .map(|(&id, name)| (name.clone(), emit_options(pool, id, &names)))
            .collect();

        map.insert("main".to_string(), emit_options(pool, root_id, &names));

        YamlFetchDocument(map)
    }
}

/// DFS that counts in-degrees of reachable non-default entries and marks cyclic ones.
fn collect_refs(
    pool: &EntryFetchOptionsPool,
    id: OptionsId,
    in_degree: &mut HashMap<OptionsId, usize>,
    stack_set: &mut HashSet<OptionsId>,
    cyclic: &mut HashSet<OptionsId>,
) {
    if id == EntryFetchOptionsPool::DEFAULT_ID {
        return;
    }

    // Back edge → cycle
    if stack_set.contains(&id) {
        cyclic.insert(id);
        return;
    }

    *in_degree.entry(id).or_insert(0) += 1;

    // Only recurse on first visit (avoids exponential blowup on shared nodes)
    if in_degree[&id] > 1 {
        return;
    }

    stack_set.insert(id);
    for rule in &pool.get(id).child_rules {
        if let Some(options_id) = rule.options_id {
            collect_refs(pool, options_id, in_degree, stack_set, cyclic);
        }
    }
    stack_set.remove(&id);
}

fn emit_options(
    pool: &EntryFetchOptionsPool,
    id: OptionsId,
    names: &HashMap<OptionsId, String>,
) -> YamlEntryFetchOptions {
    let child_rules = pool
        .get(id)
        .child_rules
        .iter()
        .map(|r| YamlChildRule {
            matcher: emit_matcher_expr(&r.matcher),
            fetch: r.options_id.and_then(|id| emit_fetch_ref(pool, id, names)),
        })
        .collect();

    YamlEntryFetchOptions { child_rules }
}

fn emit_fetch_ref(
    pool: &EntryFetchOptionsPool,
    options_id: OptionsId,
    names: &HashMap<OptionsId, String>,
) -> Option<YamlFetchRef> {
    if options_id == EntryFetchOptionsPool::DEFAULT_ID {
        return None;
    }
    if let Some(name) = names.get(&options_id) {
        return Some(YamlFetchRef::Ref(name.clone()));
    }
    // Singly-referenced, acyclic → inline
    Some(YamlFetchRef::Inline(emit_options(pool, options_id, names)))
}

fn emit_matcher_expr(expr: &ChildMatcherExpr) -> YamlMatcherExpr {
    match expr {
        ChildMatcherExpr::Matcher(m) => match m {
            ChildMatcher::Always => YamlMatcherExpr::Always,

            ChildMatcher::Relation(RelationMatcher::IndexRange { min, max }) => {
                YamlMatcherExpr::IndexRange {
                    min: *min,
                    max: *max,
                }
            }

            ChildMatcher::EntryData(d) => match d {
                EntryDataMatcher::EntryType(t) => YamlMatcherExpr::EntryType(*t),
                EntryDataMatcher::ExternalType(t) => YamlMatcherExpr::ExternalType(t.clone()),
                EntryDataMatcher::NameRegex(p) => YamlMatcherExpr::NameRegex(p.clone()),
                EntryDataMatcher::HasSource(s) => YamlMatcherExpr::HasSource(s.clone()),
                EntryDataMatcher::AppearsOn(b) => YamlMatcherExpr::AppearsOn(*b),
                EntryDataMatcher::DurationRange { min, max } => YamlMatcherExpr::DurationRange {
                    min: *min,
                    max: *max,
                },
                EntryDataMatcher::YouTube(yt) => YamlMatcherExpr::YouTube(match yt {
                    YouTubeDataMatcher::DescriptionRegex(p) => {
                        YamlYouTubeMatcher::DescriptionRegex(p.clone())
                    }
                    YouTubeDataMatcher::CategoryId(id) => {
                        YamlYouTubeMatcher::CategoryId(id.clone())
                    }
                }),
                EntryDataMatcher::MusicBrainz(mb) => YamlMatcherExpr::MusicBrainz(match mb {
                    MusicBrainzDataMatcher::ReleaseGroupPrimaryType(t) => {
                        YamlMusicBrainzMatcher::ReleaseGroupPrimaryType(t.clone())
                    }
                    MusicBrainzDataMatcher::ReleaseGroupHasSecondaryType(t) => {
                        YamlMusicBrainzMatcher::ReleaseGroupHasSecondaryType(t.clone())
                    }
                    MusicBrainzDataMatcher::ReleaseStatus(s) => {
                        YamlMusicBrainzMatcher::ReleaseStatus(s.clone())
                    }
                    MusicBrainzDataMatcher::ReleaseCountry(c) => {
                        YamlMusicBrainzMatcher::ReleaseCountry(c.clone())
                    }
                    MusicBrainzDataMatcher::RecordingIsVideo => {
                        YamlMusicBrainzMatcher::RecordingIsVideo
                    }
                }),
            },

            ChildMatcher::ChildrenSatisfy { matcher, mode } => {
                YamlMatcherExpr::ChildrenSatisfy(YamlChildrenSatisfy {
                    matcher: Box::new(emit_matcher_expr(matcher)),
                    mode: match mode {
                        QuantifierMode::Ratio { min, max } => YamlQuantifierMode::Ratio {
                            min: *min,
                            max: *max,
                        },
                        QuantifierMode::Count { min, max } => YamlQuantifierMode::Count {
                            min: *min,
                            max: *max,
                        },
                    },
                })
            }
        },

        ChildMatcherExpr::Not(inner) => YamlMatcherExpr::Not(Box::new(emit_matcher_expr(inner))),

        ChildMatcherExpr::All(exprs) => {
            YamlMatcherExpr::All(exprs.iter().map(emit_matcher_expr).collect())
        }

        ChildMatcherExpr::Any(exprs) => {
            YamlMatcherExpr::Any(exprs.iter().map(emit_matcher_expr).collect())
        }
    }
}

// ---------------------------------------------------------------------------
// Multi-file loading
// ---------------------------------------------------------------------------

/// Load a [`YamlFetchDocument`] from a file on disk, resolving all cross-file
/// `./path/to/file.yaml::entry_name` references. Cross-file cycles are
/// supported. The `"main"` entry of the entry file becomes the root — see
/// [`load_from_file_with_root`] for callers that need a different entry
/// point.
///
/// Returns the pool, root id, and a content hash of all loaded files combined.
/// The hash is stable regardless of file discovery order and can be used as a
/// cache key: if the hash matches a previously cached result, re-fetching is unnecessary.
pub async fn load_from_file(
    path: &Path,
) -> Result<(Arc<EntryFetchOptionsPool>, OptionsId, [u8; 32]), YamlConversionError> {
    load_from_file_with_root(path, "main").await
}

/// Like [`load_from_file`], but starts from `root_name` — any top-level key
/// defined in the entry file, not just the conventional `"main"` — instead
/// of a fixed root. `"main"` itself is still resolved normally when passed
/// explicitly, so `load_from_file` is exactly `load_from_file_with_root(path, "main")`.
///
/// Exists for callers that let a user pick which named entry point in a file
/// to start from (e.g. `server`'s import UI, where one `.yaml` file can
/// define several usable starting points alongside `main`) rather than
/// always taking the file's conventional root.
pub async fn load_from_file_with_root(
    path: &Path,
    root_name: &str,
) -> Result<(Arc<EntryFetchOptionsPool>, OptionsId, [u8; 32]), YamlConversionError> {
    let canonical = std::fs::canonicalize(path).map_err(|e| YamlConversionError::Io {
        path: path.to_owned(),
        source: e,
    })?;

    // Phase 1 — discover all reachable files
    let (docs, file_hashes) = discover_files(canonical.clone()).await?;

    // Phase 2 — global pass 1: pre-allocate pool slots for every entry in every file
    let mut pool = EntryFetchOptionsPool::default();
    let mut file_name_to_id: HashMap<PathBuf, HashMap<String, OptionsId>> = HashMap::new();

    for (file_path, doc) in &docs {
        let mut name_to_id: HashMap<String, OptionsId> = HashMap::new();
        for name in doc.0.keys() {
            let id = pool.insert(EntryFetchOptions::default());
            name_to_id.insert(name.clone(), id);
        }
        file_name_to_id.insert(file_path.clone(), name_to_id);
    }

    // Phase 3 — global pass 2: resolve and patch every entry
    for (file_path, doc) in &docs {
        let name_to_id = &file_name_to_id[file_path].clone();
        let dir = file_path.parent().unwrap_or(Path::new("."));
        for (name, yaml_opts) in &doc.0 {
            let id = name_to_id[name];
            let opts =
                resolve_options_multi(yaml_opts, name_to_id, dir, &file_name_to_id, &mut pool)?;
            pool.patch(id, opts);
        }
    }

    let root_id = file_name_to_id[&canonical]
        .get(root_name)
        .copied()
        .unwrap_or(EntryFetchOptionsPool::DEFAULT_ID);

    // Combine file hashes: sort by canonical path for stable ordering, then hash the hashes.
    let mut sorted_paths: Vec<&PathBuf> = file_hashes.keys().collect();
    sorted_paths.sort_unstable();
    let mut combined = Sha256::new();
    for p in sorted_paths {
        combined.update(file_hashes[p]);
    }
    let hash: [u8; 32] = combined.finalize().into();

    Ok((Arc::new(pool), root_id, hash))
}

/// BFS discovery: parse all reachable YAML files starting from `entry`.
/// Already-visited files are skipped, so cycles terminate naturally.
/// Returns the parsed documents and a per-file SHA-256 hash of each file's raw content.
async fn discover_files(
    entry: PathBuf,
) -> Result<
    (
        HashMap<PathBuf, YamlFetchDocument>,
        HashMap<PathBuf, [u8; 32]>,
    ),
    YamlConversionError,
> {
    let mut docs: HashMap<PathBuf, YamlFetchDocument> = HashMap::new();
    let mut file_hashes: HashMap<PathBuf, [u8; 32]> = HashMap::new();
    let mut queue: Vec<PathBuf> = vec![entry];

    while let Some(path) = queue.pop() {
        if docs.contains_key(&path) {
            continue;
        }

        let text = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| YamlConversionError::Io {
                path: path.clone(),
                source: e,
            })?;

        let hash: [u8; 32] = Sha256::digest(text.as_bytes()).into();
        file_hashes.insert(path.clone(), hash);

        let doc: YamlFetchDocument =
            serde_yaml_ng::from_str(&text).map_err(|e| YamlConversionError::YamlParse {
                path: path.clone(),
                source: e,
            })?;

        let dir = path.parent().unwrap_or(Path::new("."));
        for path_part in collect_cross_file_paths(&doc) {
            let abs = dir.join(&path_part);
            let canonical = std::fs::canonicalize(&abs).map_err(|e| YamlConversionError::Io {
                path: abs.clone(),
                source: e,
            })?;
            if !docs.contains_key(&canonical) {
                queue.push(canonical);
            }
        }

        docs.insert(path, doc);
    }

    Ok((docs, file_hashes))
}

/// Collect all file path parts (left of `::`) from cross-file refs in a document.
fn collect_cross_file_paths(doc: &YamlFetchDocument) -> Vec<String> {
    let mut out = Vec::new();
    for entry in doc.0.values() {
        collect_cross_file_paths_in_entry(entry, &mut out);
    }
    out
}

fn collect_cross_file_paths_in_entry(entry: &YamlEntryFetchOptions, out: &mut Vec<String>) {
    for rule in &entry.child_rules {
        match &rule.fetch {
            Some(YamlFetchRef::Ref(s)) => {
                if let Some((path_part, _)) = s.split_once("::") {
                    out.push(path_part.to_owned());
                }
            }
            Some(YamlFetchRef::Inline(inline)) => collect_cross_file_paths_in_entry(inline, out),
            None => {}
        }
    }
}

fn resolve_options_multi(
    yaml: &YamlEntryFetchOptions,
    local_name_to_id: &HashMap<String, OptionsId>,
    current_dir: &Path,
    file_name_to_id: &HashMap<PathBuf, HashMap<String, OptionsId>>,
    pool: &mut EntryFetchOptionsPool,
) -> Result<EntryFetchOptions, YamlConversionError> {
    let child_rules = yaml
        .child_rules
        .iter()
        .map(|r| {
            let options_id = resolve_fetch_ref_multi(
                &r.fetch,
                local_name_to_id,
                current_dir,
                file_name_to_id,
                pool,
            )?;
            Ok(ChildRule {
                matcher: convert_matcher_expr(&r.matcher)?,
                options_id,
            })
        })
        .collect::<Result<Vec<_>, YamlConversionError>>()?;

    Ok(EntryFetchOptions { child_rules })
}

fn resolve_fetch_ref_multi(
    fetch: &Option<YamlFetchRef>,
    local_name_to_id: &HashMap<String, OptionsId>,
    current_dir: &Path,
    file_name_to_id: &HashMap<PathBuf, HashMap<String, OptionsId>>,
    pool: &mut EntryFetchOptionsPool,
) -> Result<Option<OptionsId>, YamlConversionError> {
    match fetch {
        None => Ok(None),
        Some(YamlFetchRef::Ref(s)) => {
            if let Some((path_part, entry_name)) = s.split_once("::") {
                let abs = current_dir.join(path_part);
                let canonical =
                    std::fs::canonicalize(&abs).map_err(|e| YamlConversionError::Io {
                        path: abs.clone(),
                        source: e,
                    })?;
                file_name_to_id
                    .get(&canonical)
                    .and_then(|m| m.get(entry_name))
                    .copied()
                    .map(Some)
                    .ok_or_else(|| YamlConversionError::UnknownRef(s.clone()))
            } else {
                local_name_to_id
                    .get(s)
                    .copied()
                    .map(Some)
                    .ok_or_else(|| YamlConversionError::UnknownRef(s.clone()))
            }
        }
        Some(YamlFetchRef::Inline(inline)) => {
            let opts = resolve_options_multi(
                inline,
                local_name_to_id,
                current_dir,
                file_name_to_id,
                pool,
            )?;
            Ok(Some(pool.insert(opts)))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> (Arc<EntryFetchOptionsPool>, OptionsId) {
        let doc: YamlFetchDocument = serde_yaml_ng::from_str(yaml).expect("parse failed");
        doc.into_pool().expect("conversion failed")
    }

    fn rules(pool: &EntryFetchOptionsPool, id: OptionsId) -> &[ChildRule] {
        &pool.get(id).child_rules
    }

    // --- into_pool ---

    #[test]
    fn test_empty_document() {
        let (pool, root) = parse("{}");
        assert!(rules(&pool, root).is_empty());
    }

    #[test]
    fn test_always_no_fetch() {
        let (pool, root) = parse(
            r"
main:
  child_rules:
    - match: always
",
        );
        let r = rules(&pool, root);
        assert_eq!(r.len(), 1);
        assert!(matches!(
            r[0].matcher,
            ChildMatcherExpr::Matcher(ChildMatcher::Always)
        ));
        assert_eq!(r[0].options_id, None);
    }

    #[test]
    fn test_named_ref() {
        let (pool, root) = parse(
            r"
tracks:
  child_rules:
    - match: always
main:
  child_rules:
    - match: { entry_type: track }
      fetch: tracks
",
        );
        let r = rules(&pool, root);
        assert_eq!(r.len(), 1);
        let child_id = r[0].options_id.expect("expected Some options_id");
        assert_eq!(rules(&pool, child_id).len(), 1);
    }

    #[test]
    fn test_self_referential() {
        let (pool, root) = parse(
            r"
mv_only:
  child_rules:
    - match: { name_regex: '【MV】' }
      fetch: mv_only
main:
  child_rules:
    - match: always
      fetch: mv_only
",
        );
        let mv_id = rules(&pool, root)[0].options_id.expect("expected Some");
        let mv_rules = rules(&pool, mv_id);
        assert_eq!(mv_rules.len(), 1);
        // Self-reference: the rule inside mv_only points back to mv_only
        assert_eq!(mv_rules[0].options_id, Some(mv_id));
    }

    #[test]
    fn test_shared_ref() {
        let (pool, root) = parse(
            r"
shared:
  child_rules:
    - match: always
main:
  child_rules:
    - match: { entry_type: track }
      fetch: shared
    - match: { entry_type: release }
      fetch: shared
",
        );
        let r = rules(&pool, root);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].options_id, r[1].options_id);
        assert!(r[0].options_id.is_some());
    }

    #[test]
    fn test_inline_fetch() {
        let (pool, root) = parse(
            r"
main:
  child_rules:
    - match: always
      fetch:
        child_rules:
          - match: { name_regex: 'MV' }
",
        );
        let inline_id = rules(&pool, root)[0].options_id.expect("expected Some");
        assert_eq!(rules(&pool, inline_id).len(), 1);
    }

    #[test]
    fn test_unknown_ref_errors() {
        let doc: YamlFetchDocument = serde_yaml_ng::from_str(
            "main:\n  child_rules:\n    - match: always\n      fetch: nonexistent\n",
        )
        .unwrap();
        assert!(doc.into_pool().is_err());
    }

    #[test]
    fn test_matcher_variants() {
        let (pool, root) = parse(
            r"
main:
  child_rules:
    - match: always
    - match: { entry_type: track }
    - match: { name_regex: 'MV' }
    - match: { has_source: spotify }
    - match: { duration_range: { min: 1000, max: 5000 } }
    - match: { index_range: { max: 10 } }
    - match: { youtube: { category_id: '10' } }
    - match: { youtube: { description_regex: 'original' } }
    - match:
        children_satisfy:
          matcher: always
          mode:
            ratio: { min: 0.5 }
    - match: { not: { entry_type: artist } }
    - match:
        all:
          - { entry_type: track }
          - { name_regex: 'MV' }
    - match:
        any:
          - { entry_type: track }
          - { entry_type: release }
",
        );
        assert_eq!(rules(&pool, root).len(), 12);
    }

    // --- from_pool round-trips ---

    fn round_trip(yaml: &str) -> (Arc<EntryFetchOptionsPool>, OptionsId) {
        let doc: YamlFetchDocument = serde_yaml_ng::from_str(yaml).unwrap();
        let (pool, root_id) = doc.into_pool().unwrap();
        let emitted = YamlFetchDocument::from_pool(&pool, root_id);
        let yaml2 = serde_yaml_ng::to_string(&emitted).unwrap();
        let doc2: YamlFetchDocument = serde_yaml_ng::from_str(&yaml2).unwrap();
        doc2.into_pool().unwrap()
    }

    #[test]
    fn test_round_trip_simple() {
        let (pool, root) = round_trip(
            r"
main:
  child_rules:
    - match: always
",
        );
        assert_eq!(rules(&pool, root).len(), 1);
    }

    #[test]
    fn test_round_trip_inline() {
        let (pool, root) = round_trip(
            r"
main:
  child_rules:
    - match: always
      fetch:
        child_rules:
          - match: { name_regex: 'MV' }
",
        );
        let inline_id = rules(&pool, root)[0].options_id.expect("expected Some");
        assert_eq!(rules(&pool, inline_id).len(), 1);
    }

    #[test]
    fn test_round_trip_self_ref() {
        let (pool, root) = round_trip(
            r"
mv_only:
  child_rules:
    - match: { name_regex: '【MV】' }
      fetch: mv_only
main:
  child_rules:
    - match: always
      fetch: mv_only
",
        );
        let mv_id = rules(&pool, root)[0].options_id.expect("expected Some");
        // Still self-referential after round-trip
        assert_eq!(rules(&pool, mv_id)[0].options_id, Some(mv_id));
    }

    #[test]
    fn test_round_trip_shared() {
        let (pool, root) = round_trip(
            r"
shared:
  child_rules:
    - match: always
main:
  child_rules:
    - match: { entry_type: track }
      fetch: shared
    - match: { entry_type: release }
      fetch: shared
",
        );
        let r = rules(&pool, root);
        assert_eq!(r[0].options_id, r[1].options_id);
    }

    // --- into_pool rejects cross-file refs ---

    #[test]
    fn test_into_pool_rejects_cross_file_ref() {
        let doc: YamlFetchDocument = serde_yaml_ng::from_str(
            "main:\n  child_rules:\n    - match: always\n      fetch: \"./other.yaml::foo\"\n",
        )
        .unwrap();
        assert!(matches!(
            doc.into_pool(),
            Err(YamlConversionError::CrossFileRefInMemory(_))
        ));
    }

    // --- multi-file tests ---

    #[tokio::test]
    async fn test_cross_file_basic_ref() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.yaml");
        let b = dir.path().join("b.yaml");

        tokio::fs::write(
            &b,
            "shared:\n  child_rules:\n    - match: { entry_type: track }\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            &a,
            "main:\n  child_rules:\n    - match: always\n      fetch: \"./b.yaml::shared\"\n",
        )
        .await
        .unwrap();

        let (pool, root, _hash) = load_from_file(&a).await.expect("load failed");
        let shared_id = rules(&pool, root)[0].options_id.expect("expected Some");
        assert_eq!(rules(&pool, shared_id).len(), 1);
    }

    #[tokio::test]
    async fn test_cross_file_cycle_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.yaml");
        let b = dir.path().join("b.yaml");

        tokio::fs::write(
            &a,
            "main:\n  child_rules:\n    - match: always\n      fetch: \"./b.yaml::root\"\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            &b,
            "root:\n  child_rules:\n    - match: always\n      fetch: \"./a.yaml::main\"\n",
        )
        .await
        .unwrap();

        let (pool, root_id, _hash) = load_from_file(&a)
            .await
            .expect("cross-file cycle must not error");
        let b_opts_id = rules(&pool, root_id)[0].options_id.expect("expected Some");
        let back_id = rules(&pool, b_opts_id)[0]
            .options_id
            .expect("expected Some");
        assert_eq!(back_id, root_id);
    }

    #[tokio::test]
    async fn test_cross_file_shared_loaded_once() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.yaml");
        let b = dir.path().join("b.yaml");
        let c = dir.path().join("c.yaml");

        tokio::fs::write(
            &c,
            "entry:\n  child_rules:\n    - match: { entry_type: track }\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            &b,
            "from_c:\n  child_rules:\n    - match: always\n      fetch: \"./c.yaml::entry\"\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            &a,
            "main:\n  child_rules:\n    - match: always\n      fetch: \"./b.yaml::from_c\"\n    - match: always\n      fetch: \"./c.yaml::entry\"\n",
        )
        .await
        .unwrap();

        let (pool, root, _hash) = load_from_file(&a).await.expect("load failed");
        let r = rules(&pool, root);
        // Both rules ultimately point at c.yaml::entry — should be the same OptionsId
        let via_b = rules(&pool, r[0].options_id.expect("Some"))[0].options_id;
        let direct = r[1].options_id;
        assert_eq!(via_b, direct);
    }

    #[tokio::test]
    async fn test_cross_file_unknown_entry() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.yaml");
        let b = dir.path().join("b.yaml");

        tokio::fs::write(&b, "other:\n  child_rules:\n    - match: always\n")
            .await
            .unwrap();
        tokio::fs::write(
            &a,
            "main:\n  child_rules:\n    - match: always\n      fetch: \"./b.yaml::nonexistent\"\n",
        )
        .await
        .unwrap();

        let result = load_from_file(&a).await;
        assert!(matches!(result, Err(YamlConversionError::UnknownRef(_))));
    }

    #[tokio::test]
    async fn test_load_missing_file_returns_io_error() {
        let result = load_from_file(std::path::Path::new("/nonexistent/path/x.yaml")).await;
        assert!(matches!(result, Err(YamlConversionError::Io { .. })));
    }

    #[tokio::test]
    async fn test_file_hash_is_stable_and_content_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.yaml");
        let b = dir.path().join("b.yaml");

        let content_a =
            "main:\n  child_rules:\n    - match: always\n      fetch: \"./b.yaml::shared\"\n";
        let content_b = "shared:\n  child_rules:\n    - match: { entry_type: track }\n";

        tokio::fs::write(&a, content_a).await.unwrap();
        tokio::fs::write(&b, content_b).await.unwrap();

        let (_, _, hash1) = load_from_file(&a).await.unwrap();
        let (_, _, hash2) = load_from_file(&a).await.unwrap();
        assert_eq!(hash1, hash2, "hash must be stable across loads");

        // Modify b.yaml — hash should change
        tokio::fs::write(
            &b,
            "shared:\n  child_rules:\n    - match: { entry_type: release }\n",
        )
        .await
        .unwrap();
        let (_, _, hash3) = load_from_file(&a).await.unwrap();
        assert_ne!(
            hash1, hash3,
            "hash must change when a referenced file changes"
        );
    }

    /// The shipped configs under `config/fetch_options/` load, and a
    /// discography's tracks get the `track` set, whose children are leaves.
    #[tokio::test]
    async fn shipped_configs_load() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("config/fetch_options");
        for name in [
            "fetch_discography.yaml",
            "no_fetch_discography.yaml",
            "vtuber_fetch_discography.yaml",
        ] {
            load_from_file(&dir.join(name))
                .await
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        for name in ["fetch_discography.yaml", "vtuber_fetch_discography.yaml"] {
            let (pool, track, _) = load_from_file_with_root(&dir.join(name), "track")
                .await
                .unwrap();
            let rules = &pool.get(track).child_rules;
            assert_eq!(rules.len(), 1, "{name}");
            let leaf = rules[0].options_id.expect("track children are fetched");
            assert!(pool.get(leaf).child_rules.is_empty(), "{name}: leaf");
        }

        // `main` sends releases the artist only appears on to the leaf set.
        for name in ["fetch_discography.yaml", "vtuber_fetch_discography.yaml"] {
            let (pool, main, _) = load_from_file(&dir.join(name)).await.unwrap();
            let rule = pool
                .get(main)
                .child_rules
                .iter()
                .find(|r| {
                    matches!(
                        r.matcher,
                        ChildMatcherExpr::Matcher(ChildMatcher::EntryData(
                            EntryDataMatcher::AppearsOn(true)
                        ))
                    )
                })
                .unwrap_or_else(|| panic!("{name}: no appears_on rule"));
            let leaf = rule.options_id.expect("appearances are fetched");
            assert!(pool.get(leaf).child_rules.is_empty(), "{name}: leaf");
        }
    }
}
