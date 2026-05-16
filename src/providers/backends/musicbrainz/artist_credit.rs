use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::providers::{
    backends::musicbrainz::{SOURCE, canonicalize::artist_url, types::EXTERNAL_TYPE_ARTIST},
    std_values::StandardRoleNames,
    types::{ChildRef, Contribution, EntryType},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistCredit {
    pub name: Option<String>,
    #[serde(rename = "joinphrase", default)]
    pub join_phrase: String,
    pub artist: ArtistCreditArtist,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistCreditArtist {
    pub id: String,
    pub name: String,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

/// Convert an `artist-credit` array into `ChildRef`s suitable for use as a child source.
pub fn artist_credit_child_refs(credits: &[ArtistCredit]) -> Vec<ChildRef> {
    credits
        .iter()
        .enumerate()
        .map(|(i, credit)| {
            let mut extra = serde_json::Map::new();
            if !credit.join_phrase.is_empty() {
                extra.insert(
                    "joinphrase".into(),
                    serde_json::Value::String(credit.join_phrase.clone()),
                );
            }
            extra.insert("index".into(), serde_json::Value::Number(i.into()));
            ChildRef {
                entry_type: EntryType::Artist,
                external_type: EXTERNAL_TYPE_ARTIST.into(),
                sources: [(
                    SOURCE.into(),
                    HashSet::from([artist_url(&credit.artist.id)]),
                )]
                .into(),
                name: Some(
                    credit
                        .name
                        .clone()
                        .unwrap_or_else(|| credit.artist.name.clone()),
                ),
                contributions: vec![Contribution {
                    role: StandardRoleNames::LISTED_ARTIST.into(),
                    main_artist: true,
                    source: SOURCE.into(),
                    extra: serde_json::Value::Object(extra),
                }],
                ..Default::default()
            }
        })
        .collect()
}
