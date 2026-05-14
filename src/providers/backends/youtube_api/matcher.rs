use crate::providers::types::{CompiledYouTubeDataMatcher, EntityResult};

pub fn evaluate(matcher: &CompiledYouTubeDataMatcher, entity: &EntityResult) -> bool {
    match matcher {
        CompiledYouTubeDataMatcher::DescriptionRegex(regex) => entity
            .extra
            .get("snippet")
            .and_then(|s| s.get("description"))
            .and_then(|d| d.as_str())
            .map(|d| regex.is_match(d))
            .unwrap_or(false),
        CompiledYouTubeDataMatcher::CategoryId(id) => entity
            .extra
            .get("snippet")
            .and_then(|s| s.get("categoryId"))
            .and_then(|c| c.as_str())
            .map(|c| c == id.as_str())
            .unwrap_or(false),
    }
}
