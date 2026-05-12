use async_trait::async_trait;

use crate::providers::types::{
    CanonicalizeResult, EntityResult, EntryFetchOptions, EntryType, Error, ExternalSources,
};

pub mod backends;
pub mod std_values;
pub mod types;

#[async_trait]
pub trait CanonicalizeProvider: Send + Sync {
    async fn canonicalize(&self, url: &str) -> Option<CanonicalizeResult>;
}

#[async_trait]
pub trait FetchProvider: CanonicalizeProvider {
    async fn fetch_entry(
        &self,
        identifier: &str,
        fetch_options: EntryFetchOptions,
    ) -> Result<EntityResult, Error>;
    async fn resolve_external_source(
        &self,
        entry_type: EntryType,
        sources: &ExternalSources,
    ) -> Result<Option<ExternalSources>, Error> {
        Ok(None)
    }
}

#[async_trait]
pub trait RawFetchProvider: CanonicalizeProvider {}
