use async_trait::async_trait;
use std::{future::Future, sync::Arc};

use crate::providers::types::{
    CanonicalizeResult, EntityResult, EntryFetchOptions, EntryFetchOptionsPool, EntryType, Error,
    ExternalSources, OptionsId,
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
    async fn resolve_external_source(
        &self,
        _entry_type: EntryType,
        _sources: &ExternalSources,
    ) -> Result<Option<ExternalSources>, Error> {
        Ok(None)
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
