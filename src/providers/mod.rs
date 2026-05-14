use async_trait::async_trait;
use std::{future::Future, sync::Arc};

use crate::providers::types::{
    CanonicalizeResult, EntityResult, EntryFetchOptions, EntryFetchOptionsPool, EntryType, Error,
    ExternalSources, OptionsId,
};

pub mod backends;
pub mod fetch_options_yaml;
pub mod matcher;
pub mod std_values;
pub mod types;

#[async_trait]
pub trait CanonicalizeProvider: Send + Sync {
    async fn canonicalize(&self, url: &str) -> Option<CanonicalizeResult>;
}

#[async_trait]
pub trait FetchProvider: CanonicalizeProvider {
    async fn fetch_entry(
        self: Arc<Self>,
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

    fn raw_fetch<F, E, FR, R>(
        &self,
        url: &str,
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
