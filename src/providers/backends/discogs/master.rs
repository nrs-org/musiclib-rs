use std::{collections::HashSet, future::Future, sync::Arc};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::providers::{
    backends::discogs::{
        SOURCE,
        canonicalize::{master_url, match_master_url, release_url},
        client::DiscogsClient,
        release::ReleaseArtist,
        types::EXTERNAL_TYPE_RELEASE,
    },
    types::{
        CachedChildSource, ChildPage, ChildRef, CompiledChildMatcher, CompiledEntryDataMatcher,
        CompiledMatcherExpr, EntityResult, EntrySpecificData, EntryType, Error, PageFetcher,
        PaginatedChildSource, Tribool, default_eval_leaf, static_eval_expr,
    },
};

use super::release::artist_child_refs;

// --- API response types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MasterResponse {
    pub id: u64,
    pub title: String,
    pub year: Option<u32>,
    #[serde(default)]
    pub artists: Vec<ReleaseArtist>,
    pub main_release: Option<u64>,
    pub versions_url: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MasterVersionsResponse {
    pub pagination: Pagination,
    pub versions: Vec<MasterVersion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pagination {
    pub page: u32,
    pub pages: u32,
    pub items: u32,
    pub per_page: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MasterVersion {
    pub id: u64,
    pub title: Option<String>,
    pub released: Option<String>,
    pub country: Option<String>,
}

// --- Constants ---

const VERSIONS_PER_PAGE: u32 = 100;

// --- Page fetchers ---

struct MasterVersionsPageFetcher {
    client: DiscogsClient,
    master_id: String,
}

#[async_trait::async_trait]
impl PageFetcher for MasterVersionsPageFetcher {
    async fn fetch_page(&mut self, page_token: Option<&str>) -> Result<ChildPage, Error> {
        let page: u32 = page_token.and_then(|t| t.parse().ok()).unwrap_or(1);
        let per_page_str = VERSIONS_PER_PAGE.to_string();
        let page_str = page.to_string();

        let response = self
            .client
            .get::<MasterVersionsResponse, _, _, Error, _>(
                &format!("masters/{}/versions", self.master_id),
                &[
                    ("per_page", per_page_str.as_str()),
                    ("page", page_str.as_str()),
                ],
                |r| {
                    let r = r.clone();
                    async move { Ok(r) }
                },
            )
            .await?;

        let next_page = if response.pagination.page < response.pagination.pages {
            Some((response.pagination.page + 1).to_string())
        } else {
            None
        };

        let children = response
            .versions
            .into_iter()
            .map(|v| ChildRef {
                entry_type: EntryType::Release,
                external_type: EXTERNAL_TYPE_RELEASE.into(),
                sources: [(
                    SOURCE.into(),
                    HashSet::from([release_url(&v.id.to_string())]),
                )]
                .into(),
                name: v.title,
                ..Default::default()
            })
            .collect();

        Ok(ChildPage {
            children,
            next_page_token: next_page,
        })
    }
}

fn release_eval(expr: &CompiledMatcherExpr) -> Tribool {
    static_eval_expr(expr, &|matcher| match matcher {
        CompiledChildMatcher::EntryData(CompiledEntryDataMatcher::EntryType(t)) => {
            (*t == EntryType::Release).into()
        }
        _ => default_eval_leaf(matcher),
    })
}

// --- Public API ---

pub async fn get_master_raw<T, F, R, E, FR>(
    client: &DiscogsClient,
    url: &str,
    callback: F,
) -> Result<R, E>
where
    T: std::any::Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
    F: FnOnce(&T, &str) -> FR,
    E: From<Error> + Send + 'static,
    FR: Future<Output = Result<R, E>> + Send + 'static,
{
    let id = match_master_url(url).expect("Invalid Discogs master URL");
    client
        .get::<T, _, _, _, _>(&format!("masters/{id}"), &[], |value| callback(value, id))
        .await
}

pub async fn get_master(client: &DiscogsClient, url: &str) -> Result<EntityResult<()>, Error> {
    get_master_raw::<MasterResponse, _, _, Error, _>(client, url, |m, id| {
        let canonical = master_url(id);

        let release_date = m.year.map(|y| format!("{y}-XX-XX XX:XX:XX"));

        let artist_refs = artist_child_refs(&m.artists);

        let versions_source = PaginatedChildSource::new(Box::new(MasterVersionsPageFetcher {
            client: client.clone(),
            master_id: id.to_string(),
        }))
        .with_static_eval(release_eval);

        let result = Ok(EntityResult {
            release_date,
            sources: [(SOURCE.into(), HashSet::from([canonical]))].into(),
            extra: m.extra.clone(),
            specific_data: EntrySpecificData::ReleaseGroup { primary_type: None },
            children: vec![
                Arc::new(CachedChildSource::from_children(artist_refs)),
                Arc::new(CachedChildSource::new(Box::new(versions_source))),
            ],
            aliases: vec![crate::providers::types::Alias {
                name: m.title.clone(),
                source: SOURCE.into(),
                primary: true,
                ..Default::default()
            }],
        });
        async move { result }
    })
    .await
}
