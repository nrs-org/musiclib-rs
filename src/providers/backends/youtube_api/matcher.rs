use regex::Regex;

use crate::providers::{
    matcher::BackendMatcherEvaluator,
    types::{EntityResult, EntryDataMatcher, YouTubeDataMatcher},
};

pub struct YouTubeMatcherEvaluator;

impl BackendMatcherEvaluator for YouTubeMatcherEvaluator {
    fn evaluate(&self, matcher: &EntryDataMatcher, entity: &EntityResult) -> Option<bool> {
        let EntryDataMatcher::YouTube(yt) = matcher else {
            return None;
        };
        Some(match yt {
            YouTubeDataMatcher::DescriptionRegex(pattern) => entity
                .extra
                .get("snippet")
                .and_then(|s| s.get("description"))
                .and_then(|d| d.as_str())
                .map(|d| Regex::new(pattern).map(|r| r.is_match(d)).unwrap_or(false))
                .unwrap_or(false),
            YouTubeDataMatcher::CategoryId(id) => entity
                .extra
                .get("snippet")
                .and_then(|s| s.get("categoryId"))
                .and_then(|c| c.as_str())
                .map(|c| c == id.as_str())
                .unwrap_or(false),
        })
    }
}
