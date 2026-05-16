use crate::providers::types::{CompiledMusicBrainzDataMatcher, EntityResult};

pub fn evaluate(matcher: &CompiledMusicBrainzDataMatcher, entity: &EntityResult) -> bool {
    match matcher {
        CompiledMusicBrainzDataMatcher::ReleaseGroupPrimaryType(t) => {
            entity.extra.get("primary-type").and_then(|v| v.as_str()) == Some(t.as_str())
        }
        CompiledMusicBrainzDataMatcher::ReleaseGroupHasSecondaryType(t) => entity
            .extra
            .get("secondary-types")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().any(|v| v.as_str() == Some(t.as_str())))
            .unwrap_or(false),
        CompiledMusicBrainzDataMatcher::ReleaseStatus(s) => {
            entity.extra.get("status").and_then(|v| v.as_str()) == Some(s.as_str())
        }
        CompiledMusicBrainzDataMatcher::ReleaseCountry(c) => {
            entity.extra.get("country").and_then(|v| v.as_str()) == Some(c.as_str())
        }
        CompiledMusicBrainzDataMatcher::RecordingIsVideo => entity
            .extra
            .get("video")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}
