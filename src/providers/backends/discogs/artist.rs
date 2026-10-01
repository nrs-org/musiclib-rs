use std::{collections::HashSet, future::Future, sync::Arc};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tracing::warn;

use crate::providers::{
    backends::discogs::{
        SOURCE,
        canonicalize::{artist_url, master_url, match_artist_url, release_url},
        client::DiscogsClient,
        types::{EXTERNAL_TYPE_MASTER, EXTERNAL_TYPE_RELEASE},
    },
    std_values::StandardProviderKeys,
    types::{
        Alias, CachedChildSource, ChildPage, ChildRef, CompiledChildMatcher,
        CompiledEntryDataMatcher, CompiledMatcherExpr, EntityResult, EntrySpecificData, EntryType,
        Error, PageFetcher, PaginatedChildSource, Tribool, default_eval_leaf, static_eval_expr,
    },
};

// --- API response types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ArtistResponse {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub namevariations: Vec<String>,
    #[serde(default)]
    pub urls: Vec<String>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ArtistReleasesResponse {
    pub pagination: super::master::Pagination,
    pub releases: Vec<ArtistRelease>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistRelease {
    pub id: u64,
    pub title: Option<String>,
    pub year: Option<u32>,
    /// "master", "release", etc.
    #[serde(rename = "type")]
    pub release_type: Option<String>,
    pub role: Option<String>,
}

// --- Constants ---

pub(crate) const RELEASES_PER_PAGE: u32 = 100;

// --- Page fetchers ---

struct ArtistReleasesPageFetcher {
    client: DiscogsClient,
    artist_id: String,
}

#[async_trait::async_trait]
impl PageFetcher for ArtistReleasesPageFetcher {
    async fn fetch_page(&mut self, page_token: Option<&str>) -> Result<ChildPage, Error> {
        let page: u32 = page_token.and_then(|t| t.parse().ok()).unwrap_or(1);
        let per_page_str = RELEASES_PER_PAGE.to_string();
        let page_str = page.to_string();

        let response = self
            .client
            .get::<ArtistReleasesResponse, _, _, Error, _>(
                &format!("artists/{}/releases", self.artist_id),
                &[
                    ("per_page", per_page_str.as_str()),
                    ("page", page_str.as_str()),
                    ("sort", "year"),
                    ("sort_order", "asc"),
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
            .releases
            .into_iter()
            .map(|rel| {
                let is_master = rel.release_type.as_deref() == Some("master");
                let (entry_type, external_type, url) = if is_master {
                    (
                        EntryType::ReleaseGroup,
                        EXTERNAL_TYPE_MASTER.into(),
                        master_url(&rel.id.to_string()),
                    )
                } else {
                    (
                        EntryType::Release,
                        EXTERNAL_TYPE_RELEASE.into(),
                        release_url(&rel.id.to_string()),
                    )
                };
                ChildRef {
                    entry_type,
                    external_type,
                    sources: [(SOURCE.into(), HashSet::from([url]))].into(),
                    name: rel.title,
                    appears_on: matches!(
                        rel.role.as_deref(),
                        Some("Appearance" | "TrackAppearance")
                    ),
                    ..Default::default()
                }
            })
            .collect();

        Ok(ChildPage {
            children,
            next_page_token: next_page,
        })
    }
}

fn release_or_release_group_eval(expr: &CompiledMatcherExpr) -> Tribool {
    static_eval_expr(expr, &|matcher| match matcher {
        CompiledChildMatcher::EntryData(CompiledEntryDataMatcher::EntryType(t)) => {
            (*t == EntryType::Release || *t == EntryType::ReleaseGroup).into()
        }
        CompiledChildMatcher::EntryData(CompiledEntryDataMatcher::ExternalType(t)) => {
            // Items are always one of these two; can never be True (mixed), but can be False.
            if t != EXTERNAL_TYPE_RELEASE && t != EXTERNAL_TYPE_MASTER {
                Tribool::False
            } else {
                Tribool::Indeterminate
            }
        }
        _ => default_eval_leaf(matcher),
    })
}

// --- Public API ---

pub async fn get_artist_raw<T, F, R, E, FR>(
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
    let id = match_artist_url(url).expect("Invalid Discogs artist URL");
    client
        .get::<T, _, _, _, _>(&format!("artists/{id}"), &[], |value| callback(value, id))
        .await
}

pub async fn get_artist_releases_raw<T, F, R, E, FR>(
    client: &DiscogsClient,
    url: &str,
    callback: F,
) -> Result<R, E>
where
    T: std::any::Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
    F: FnOnce(&T) -> FR,
    E: From<Error> + Send + 'static,
    FR: Future<Output = Result<R, E>> + Send + 'static,
{
    let id = match_artist_url(url).expect("Invalid Discogs artist URL");
    let per_page_str = RELEASES_PER_PAGE.to_string();
    client
        .get::<T, _, _, _, _>(
            &format!("artists/{id}/releases"),
            &[
                ("per_page", per_page_str.as_str()),
                ("page", "1"),
                ("sort", "year"),
                ("sort_order", "asc"),
            ],
            |value| callback(value),
        )
        .await
}

pub async fn get_artist(client: &DiscogsClient, url: &str) -> Result<EntityResult<()>, Error> {
    get_artist_raw::<ArtistResponse, _, _, Error, _>(client, url, |a, id| {
        let canonical = artist_url(id);

        let mut aliases = vec![Alias {
            name: a.name.clone(),
            source: SOURCE.into(),
            primary: true,
            ..Default::default()
        }];
        // Generic composer-credit buckets ("Traditional", "Various", "Folk", …)
        // carry Discogs "name variations" lists running into the thousands —
        // every distinct work ever credited to them, not real aliases of one
        // artist. This cap is deliberately generous: it's a ceiling against
        // that pathological volume (and the SQLite bound-parameter crash it
        // caused), not an attempt to classify "generic bucket" vs "real
        // artist" by count alone — a genuinely prolific/long-credited act
        // (e.g. a hit-factory production team spelled a few hundred
        // different ways across decades of liner notes) can legitimately
        // reach into the hundreds without being bucket noise.
        const MAX_NAME_VARIATIONS: usize = 1000;
        if a.namevariations.len() > MAX_NAME_VARIATIONS {
            warn!(
                artist = %a.name,
                url,
                count = a.namevariations.len(),
                "discogs artist has an implausibly large name-variations list (likely a generic composer-credit bucket like \"Traditional\") — dropping variations",
            );
        } else {
            for variation in &a.namevariations {
                aliases.push(Alias {
                    name: variation.clone(),
                    source: SOURCE.into(),
                    primary: false,
                    ..Default::default()
                });
            }
        }

        let releases_source = PaginatedChildSource::new(Box::new(ArtistReleasesPageFetcher {
            client: client.clone(),
            artist_id: id.to_string(),
        }))
        .with_static_eval(release_or_release_group_eval);

        let mut sources: crate::providers::types::ExternalSources =
            [(SOURCE.into(), HashSet::from([canonical]))].into();
        for url in &a.urls {
            sources
                .0
                .entry(StandardProviderKeys::UNKNOWN_URL.into())
                .or_default()
                .insert(url.clone());
        }

        let result = Ok(EntityResult {
            release_date: None,
            sources,
            extra: a.extra.clone(),
            specific_data: EntrySpecificData::Artist,
            children: vec![Arc::new(CachedChildSource::new(Box::new(releases_source)))],
            aliases,
        });
        async move { result }
    })
    .await
}

#[cfg(test)]
mod tests {
    use crate::providers::{std_values::StandardProviderKeys, types::child_next};
    use std::{collections::HashSet, sync::Arc};

    use http::Method;

    use crate::{
        providers::{
            backends::discogs::{
                SOURCE,
                artist::{ArtistReleasesResponse, ArtistResponse, RELEASES_PER_PAGE, get_artist},
                client::DiscogsClient,
                types::{EXTERNAL_TYPE_MASTER, EXTERNAL_TYPE_RELEASE},
            },
            types::{ChildSource, EntrySpecificData, EntryType},
        },
        test_utils::MockHttpClient,
    };

    fn artist_api_url(id: &str) -> String {
        DiscogsClient::build_url(&format!("artists/{id}"), &[])
    }

    fn artist_releases_url(id: &str) -> String {
        let per_page = RELEASES_PER_PAGE.to_string();
        DiscogsClient::build_url(
            &format!("artists/{id}/releases"),
            &[
                ("per_page", per_page.as_str()),
                ("page", "1"),
                ("sort", "year"),
                ("sort_order", "asc"),
            ],
        )
    }

    #[tokio::test]
    async fn test_get_artist_watame() -> anyhow::Result<()> {
        let id = "11811530";
        let mut http_client = MockHttpClient::new();
        http_client.add_route_json::<ArtistResponse>(
            Method::GET,
            &artist_api_url(id),
            include_str!("./artist_watame.json"),
        );
        http_client.add_route_json::<ArtistReleasesResponse>(
            Method::GET,
            &artist_releases_url(id),
            include_str!("./artist_watame_releases.json"),
        );

        let client = DiscogsClient::new_with_client(Arc::new(http_client), None)?;
        let artist = get_artist(&client, &format!("https://www.discogs.com/artist/{id}")).await?;

        // Specific data
        assert!(matches!(artist.specific_data, EntrySpecificData::Artist));

        // Sources: canonical URL + external URLs
        assert_eq!(
            artist.sources.get(SOURCE).unwrap(),
            &HashSet::from(["https://www.discogs.com/artist/11811530".to_string()])
        );
        let url_sources = artist
            .sources
            .get(StandardProviderKeys::UNKNOWN_URL)
            .unwrap();
        assert!(url_sources.contains("https://twitter.com/tsunomakiwatame"));
        assert!(url_sources.contains("https://www.youtube.com/channel/UCqm3BQLlJfvkTsX_hvm0UmA"));

        // Aliases: primary name + "Tsunomaki Watame" namevariation
        assert!(
            artist
                .aliases
                .iter()
                .any(|a| a.name == "角巻わため" && a.primary)
        );
        assert!(
            artist
                .aliases
                .iter()
                .any(|a| a.name == "Tsunomaki Watame" && !a.primary)
        );

        // 1 child source: paginated releases+masters
        assert_eq!(artist.children.len(), 1);

        let mut releases = artist.children[0].cursor();

        // First item: 愛昧ショコラーテ (type "release", year 2020)
        let (first, _) = child_next(&mut releases)
            .await?
            .expect("expected first release");
        assert_eq!(first.entry_type, EntryType::Release);
        assert_eq!(first.external_type.as_ref(), EXTERNAL_TYPE_RELEASE);
        assert_eq!(first.name.as_deref(), Some("愛昧ショコラーテ"));
        assert_eq!(
            first.sources.get(SOURCE).unwrap(),
            &HashSet::from(["https://www.discogs.com/release/24550217".to_string()])
        );

        // Advance to the 8th item (index 7): Hololive Summer 2022 (type "master")
        for _ in 2..=7 {
            child_next(&mut releases).await?.expect("expected release");
        }
        let (master_item, _) = child_next(&mut releases)
            .await?
            .expect("expected master item");
        assert_eq!(master_item.entry_type, EntryType::ReleaseGroup);
        assert_eq!(master_item.external_type.as_ref(), EXTERNAL_TYPE_MASTER);
        assert_eq!(master_item.name.as_deref(), Some("Hololive Summer 2022"));
        assert_eq!(
            master_item.sources.get(SOURCE).unwrap(),
            &HashSet::from(["https://www.discogs.com/master/3004283".to_string()])
        );

        // 13 total items, no next page
        for _ in 9..=13 {
            child_next(&mut releases).await?.expect("expected release");
        }
        assert!(child_next(&mut releases).await?.is_none());

        // Releases she only appears on (role Appearance/TrackAppearance) are flagged.
        let mut replay = artist.children[0].cursor();
        let mut appears_on = 0;
        while let Some((release, _)) = child_next(&mut replay).await? {
            appears_on += usize::from(release.appears_on);
        }
        assert_eq!(appears_on, 8);

        Ok(())
    }
}
