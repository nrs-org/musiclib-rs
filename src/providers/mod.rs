use async_trait::async_trait;
use std::{future::Future, sync::Arc};

use crate::providers::types::{
    CanonicalizeResult, EntityResult, EntryFetchOptions, EntryFetchOptionsPool, EntrySpecificData,
    EntryType, Error, ExternalSources, OptionsId,
};

pub mod backends;
pub mod fetch_options_yaml;
pub mod matcher;
pub mod registry;
pub mod std_values;
pub mod types;

#[async_trait]
pub trait CanonicalizeProvider: Send + Sync {
    /// Attempt to canonicalize `identifier` of the given `source_key` type.
    ///
    /// `source_key` describes the *format* of the identifier — e.g. `"unknown_url"`
    /// for an arbitrary URL, `"isrc"` for an ISRC code. It is not an external
    /// type: `"youtube:video"` etc. are entity-kind labels (carried in
    /// [`CanonicalizeResult::external_type`]), not source keys.
    ///
    /// Each backend returns `None` for identifier formats it doesn't recognise.
    async fn canonicalize(&self, source_key: &str, identifier: &str) -> Option<CanonicalizeResult>;
}

#[async_trait]
pub trait FetchProvider: CanonicalizeProvider {
    /// Fetch a full entity.
    ///
    /// `source_key` must be this provider's own source key (returned in
    /// [`CanonicalizeResult::canonical_source_key`]). `identifier` is the
    /// canonical identifier returned by the same call. Backends derive the
    /// entity kind internally — typically by re-canonicalizing the identifier
    /// — so the public API doesn't need to surface `external_type`.
    async fn fetch_entry(
        self: Arc<Self>,
        source_key: &str,
        identifier: &str,
        pool: Arc<EntryFetchOptionsPool>,
        root_id: OptionsId,
    ) -> Result<EntityResult, Error>;
    /// Enrich an entity's sources with IDs in this provider's own namespace
    /// (cross-provider linking, e.g. ISRC -> Spotify track).
    async fn resolve_external_source(
        &self,
        _ctx: &ResolveContext<'_>,
    ) -> Result<Option<ExternalSources>, Error> {
        Ok(None)
    }
}

/// Two recordings sharing an ISRC whose lengths differ by more than this are
/// different versions: labels reuse an ISRC for an edit or a remaster.
pub const ISRC_DURATION_TOLERANCE_MS: i64 = 5_000;

/// The entity a [`FetchProvider::resolve_external_source`] call cross-links.
pub struct ResolveContext<'a> {
    pub specific_data: &'a EntrySpecificData,
    pub sources: &'a ExternalSources,
}

impl ResolveContext<'_> {
    pub fn entry_type(&self) -> EntryType {
        self.specific_data.entry_type()
    }

    /// Whether a recording found through this entity's ISRC, `candidate_ms`
    /// long, is close enough in length to be the same version. True when
    /// either side's length is unknown.
    pub fn isrc_duration_matches(&self, candidate_ms: Option<i64>) -> bool {
        let EntrySpecificData::Track { duration_ms, .. } = self.specific_data else {
            return true;
        };
        let Some(candidate) = candidate_ms else {
            return true;
        };
        duration_ms.is_empty()
            || duration_ms
                .iter()
                .any(|d| (d - candidate).abs() <= ISRC_DURATION_TOLERANCE_MS)
    }
}

pub trait RawFetchProvider: CanonicalizeProvider {
    fn name() -> &'static str;

    /// Fetch the raw API payload for an entity. Takes a `source_key` /
    /// `identifier` pair just like [`FetchProvider::fetch_entry`]; the backend
    /// derives entity kind internally.
    fn raw_fetch<F, E, FR, R>(
        &self,
        source_key: &str,
        identifier: &str,
        fetch_options: EntryFetchOptions,
        path_key: &str,
        callback: F,
    ) -> impl Future<Output = Result<R, E>> + Send
    where
        F: FnOnce(&serde_json::Value) -> FR + Send,
        E: From<Error> + Send + 'static,
        FR: Future<Output = Result<R, E>> + Send + 'static,
        R: Send;
}

pub trait TryDefault {
    type Error;
    fn try_default() -> Result<Self, Error>
    where
        Self: Sized;
}

#[cfg(test)]
mod tests {
    use super::{ExternalSources, ResolveContext};
    use crate::providers::types::EntrySpecificData;

    fn track(duration_ms: Vec<i64>) -> EntrySpecificData {
        EntrySpecificData::Track {
            duration_ms,
            positions: Default::default(),
            songs: vec![],
        }
    }

    #[test]
    fn isrc_duration_guard() {
        let sources = ExternalSources::default();
        // MB 4:50 vs a Spotify 4:58 edit under the same ISRC.
        let data = track(vec![290_000]);
        let ctx = ResolveContext {
            specific_data: &data,
            sources: &sources,
        };
        assert!(ctx.isrc_duration_matches(Some(293_000)));
        assert!(!ctx.isrc_duration_matches(Some(298_000)));
        assert!(ctx.isrc_duration_matches(None), "unknown candidate length");

        // Any of the recording's per-release lengths may match.
        let data = track(vec![290_000, 298_000]);
        let ctx = ResolveContext {
            specific_data: &data,
            sources: &sources,
        };
        assert!(ctx.isrc_duration_matches(Some(298_000)));

        let data = track(vec![]);
        let ctx = ResolveContext {
            specific_data: &data,
            sources: &sources,
        };
        assert!(ctx.isrc_duration_matches(Some(1)), "unknown own length");
    }
}
