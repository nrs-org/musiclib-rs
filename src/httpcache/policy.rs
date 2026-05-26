use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachePolicy {
    pub status_rules: Vec<StatusCacheRule>,
}

impl CachePolicy {
    pub fn resolve(&self, status: u16) -> Option<&ResponseCachePolicy> {
        self.status_rules
            .iter()
            .find(|rule| rule.status.matches(status))
            .and_then(|rule| rule.policy.as_ref())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusCacheRule {
    pub status: StatusMatcher,
    /// `None` means don't cache responses matching this status.
    pub policy: Option<ResponseCachePolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StatusMatcher {
    /// `"any"`
    Any(AnyMatcher),
    /// `"2xx"`, `"3xx"`, `"4xx"`, `"5xx"`
    Class(StatusClass),
    /// Exact status code, e.g. `529`
    Exact(u16),
}

impl StatusMatcher {
    pub fn matches(&self, status: u16) -> bool {
        match self {
            StatusMatcher::Any(_) => true,
            StatusMatcher::Class(c) => c.matches(status),
            StatusMatcher::Exact(code) => *code == status,
        }
    }
}

/// Deserializes from the string `"any"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename = "any")]
pub struct AnyMatcher;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StatusClass {
    #[serde(rename = "2xx")]
    Success,
    #[serde(rename = "3xx")]
    Redirect,
    #[serde(rename = "4xx")]
    ClientError,
    #[serde(rename = "5xx")]
    ServerError,
}

impl StatusClass {
    pub fn matches(&self, status: u16) -> bool {
        match self {
            StatusClass::Success => (200..300).contains(&status),
            StatusClass::Redirect => (300..400).contains(&status),
            StatusClass::ClientError => (400..500).contains(&status),
            StatusClass::ServerError => (500..600).contains(&status),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseCachePolicy {
    pub ttl: TtlPolicy,
    #[serde(deserialize_with = "crate::duration::deserialize")]
    pub swr: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TtlPolicy {
    /// Fixed TTL, e.g. `ttl: "1h"`
    Fixed(#[serde(deserialize_with = "crate::duration::deserialize")] Duration),
    /// Exponential backoff TTL.
    Backoff {
        #[serde(deserialize_with = "crate::duration::deserialize")]
        initial: Duration,
        multiplier: f64,
        #[serde(deserialize_with = "crate::duration::deserialize")]
        max: Duration,
    },
}

impl TtlPolicy {
    /// Compute the effective TTL given the number of consecutive errors so far
    /// (before this response). For `Fixed`, `error_count` is ignored.
    pub fn compute(&self, error_count: u32) -> Duration {
        match self {
            TtlPolicy::Fixed(d) => *d,
            TtlPolicy::Backoff {
                initial,
                multiplier,
                max,
            } => {
                let secs = initial.as_secs_f64() * multiplier.powi(error_count as i32);
                Duration::from_secs_f64(secs.min(max.as_secs_f64()))
            }
        }
    }
}
