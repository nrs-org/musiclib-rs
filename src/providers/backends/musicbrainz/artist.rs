use std::{collections::HashSet, future::Future, sync::Arc};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::providers::{
    backends::musicbrainz::{
        SOURCE,
        canonicalize::{
            artist_url, match_artist_url, recording_url, release_group_url, release_url,
        },
        client::MusicBrainzClient,
        types::{EXTERNAL_TYPE_RECORDING, EXTERNAL_TYPE_RELEASE, EXTERNAL_TYPE_RELEASE_GROUP},
        url::{UrlResource, url_source_key},
    },
    types::{
        Alias, CachedChildSource, ChildPage, ChildRef, CompiledChildMatcher,
        CompiledEntryDataMatcher, CompiledMatcherExpr, EntityResult, EntrySpecificData, EntryType,
        Error, PageFetcher, PaginatedChildSource, Tribool, default_eval_leaf, static_eval_expr,
    },
};

// --- API response types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ArtistResponse {
    pub id: String,
    pub name: String,
    #[serde(rename = "sort-name")]
    pub sort_name: Option<String>,
    pub disambiguation: Option<String>,
    #[serde(rename = "type")]
    pub artist_type: Option<String>,
    #[serde(default)]
    pub aliases: Vec<ArtistAlias>,
    #[serde(default)]
    pub relations: Vec<ArtistRelation>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistRelation {
    #[serde(rename = "type")]
    pub relation_type: String,
    #[serde(rename = "target-type")]
    pub target_type: String,
    pub recording: Option<ArtistRelationRecording>,
    pub(crate) url: Option<UrlResource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistRelationRecording {
    pub id: String,
    pub title: Option<String>,
    pub length: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistAlias {
    pub name: Option<String>,
    pub locale: Option<String>,
    #[serde(rename = "type")]
    pub alias_type: Option<String>,
    pub primary: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ArtistReleaseGroupsResponse {
    #[serde(rename = "release-group-count")]
    pub release_group_count: u32,
    #[serde(rename = "release-groups")]
    pub release_groups: Vec<ArtistReleaseGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistReleaseGroup {
    pub id: String,
    pub title: Option<String>,
    #[serde(rename = "primary-type")]
    pub primary_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ArtistReleasesResponse {
    #[serde(rename = "release-count")]
    pub release_count: u32,
    pub releases: Vec<ArtistRelease>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistRelease {
    pub id: String,
    pub title: Option<String>,
    pub date: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ArtistRecordingsResponse {
    #[serde(rename = "recording-count")]
    pub recording_count: u32,
    pub recordings: Vec<ArtistRecording>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistRecording {
    pub id: String,
    pub title: Option<String>,
    pub length: Option<i64>,
}

// --- Constants ---

pub(crate) const ARTIST_PARTS: &str = "aliases+recording-rels+url-rels";
pub(crate) const RELEASE_GROUPS_LIMIT: u32 = 100;
pub(crate) const RELEASES_LIMIT: u32 = 100;
pub(crate) const RECORDINGS_LIMIT: u32 = 100;

// --- Page fetchers ---

// Offset-based page fetcher shared pattern:
// stores the artist MBID and the current offset.
// `next_page_token` is the next offset as a decimal string, or None when exhausted.
struct ArtistReleaseGroupsPageFetcher {
    client: MusicBrainzClient,
    artist_id: String,
}

#[async_trait::async_trait]
impl PageFetcher for ArtistReleaseGroupsPageFetcher {
    async fn fetch_page(&mut self, page_token: Option<&str>) -> Result<ChildPage, Error> {
        let offset: u32 = page_token.and_then(|t| t.parse().ok()).unwrap_or(0);
        let limit_str = RELEASE_GROUPS_LIMIT.to_string();
        let offset_str = offset.to_string();
        let response = self
            .client
            .get::<ArtistReleaseGroupsResponse, _, _, Error, _>(
                "release-group",
                &[
                    ("artist", self.artist_id.as_str()),
                    ("limit", limit_str.as_str()),
                    ("offset", offset_str.as_str()),
                ],
                |r| {
                    let r = r.clone();
                    async move { Ok(r) }
                },
            )
            .await?;

        let fetched = response.release_groups.len() as u32;
        let next_offset = offset + fetched;
        let next_page_token = if next_offset < response.release_group_count {
            Some(next_offset.to_string())
        } else {
            None
        };

        let children = response
            .release_groups
            .into_iter()
            .map(|rg| ChildRef {
                entry_type: EntryType::ReleaseGroup,
                external_type: EXTERNAL_TYPE_RELEASE_GROUP.into(),
                sources: [(SOURCE.into(), HashSet::from([release_group_url(&rg.id)]))].into(),
                name: rg.title,
                ..Default::default()
            })
            .collect();

        Ok(ChildPage {
            children,
            next_page_token,
        })
    }
}

struct ArtistReleasesPageFetcher {
    client: MusicBrainzClient,
    artist_id: String,
}

#[async_trait::async_trait]
impl PageFetcher for ArtistReleasesPageFetcher {
    async fn fetch_page(&mut self, page_token: Option<&str>) -> Result<ChildPage, Error> {
        let offset: u32 = page_token.and_then(|t| t.parse().ok()).unwrap_or(0);
        let limit_str = RELEASES_LIMIT.to_string();
        let offset_str = offset.to_string();
        let response = self
            .client
            .get::<ArtistReleasesResponse, _, _, Error, _>(
                "release",
                &[
                    ("artist", self.artist_id.as_str()),
                    ("limit", limit_str.as_str()),
                    ("offset", offset_str.as_str()),
                ],
                |r| {
                    let r = r.clone();
                    async move { Ok(r) }
                },
            )
            .await?;

        let fetched = response.releases.len() as u32;
        let next_offset = offset + fetched;
        let next_page_token = if next_offset < response.release_count {
            Some(next_offset.to_string())
        } else {
            None
        };

        let children = response
            .releases
            .into_iter()
            .map(|rel| ChildRef {
                entry_type: EntryType::Release,
                external_type: EXTERNAL_TYPE_RELEASE.into(),
                sources: [(SOURCE.into(), HashSet::from([release_url(&rel.id)]))].into(),
                name: rel.title,
                ..Default::default()
            })
            .collect();

        Ok(ChildPage {
            children,
            next_page_token,
        })
    }
}

struct ArtistRecordingsPageFetcher {
    client: MusicBrainzClient,
    artist_id: String,
}

#[async_trait::async_trait]
impl PageFetcher for ArtistRecordingsPageFetcher {
    async fn fetch_page(&mut self, page_token: Option<&str>) -> Result<ChildPage, Error> {
        let offset: u32 = page_token.and_then(|t| t.parse().ok()).unwrap_or(0);
        let limit_str = RECORDINGS_LIMIT.to_string();
        let offset_str = offset.to_string();
        let response = self
            .client
            .get::<ArtistRecordingsResponse, _, _, Error, _>(
                "recording",
                &[
                    ("artist", self.artist_id.as_str()),
                    ("limit", limit_str.as_str()),
                    ("offset", offset_str.as_str()),
                ],
                |r| {
                    let r = r.clone();
                    async move { Ok(r) }
                },
            )
            .await?;

        let fetched = response.recordings.len() as u32;
        let next_offset = offset + fetched;
        let next_page_token = if next_offset < response.recording_count {
            Some(next_offset.to_string())
        } else {
            None
        };

        let children = response
            .recordings
            .into_iter()
            .map(|rec| ChildRef {
                entry_type: EntryType::Track,
                external_type: EXTERNAL_TYPE_RECORDING.into(),
                sources: [(SOURCE.into(), HashSet::from([recording_url(&rec.id)]))].into(),
                name: rec.title,
                ..Default::default()
            })
            .collect();

        Ok(ChildPage {
            children,
            next_page_token,
        })
    }
}

// --- Static eval helpers ---

fn release_group_eval(expr: &CompiledMatcherExpr) -> Tribool {
    static_eval_expr(expr, &|matcher| match matcher {
        CompiledChildMatcher::EntryData(CompiledEntryDataMatcher::EntryType(t)) => {
            (*t == EntryType::ReleaseGroup).into()
        }
        _ => default_eval_leaf(matcher),
    })
}

fn release_eval(expr: &CompiledMatcherExpr) -> Tribool {
    static_eval_expr(expr, &|matcher| match matcher {
        CompiledChildMatcher::EntryData(CompiledEntryDataMatcher::EntryType(t)) => {
            (*t == EntryType::Release).into()
        }
        _ => default_eval_leaf(matcher),
    })
}

fn recording_eval(expr: &CompiledMatcherExpr) -> Tribool {
    static_eval_expr(expr, &|matcher| match matcher {
        CompiledChildMatcher::EntryData(CompiledEntryDataMatcher::EntryType(t)) => {
            (*t == EntryType::Track).into()
        }
        _ => default_eval_leaf(matcher),
    })
}

// --- Public API ---

pub async fn get_artist_raw<T, F, R, E, FR>(
    client: &MusicBrainzClient,
    url: &str,
    callback: F,
) -> Result<R, E>
where
    T: std::any::Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
    F: FnOnce(&T, &str) -> FR,
    E: From<Error> + Send + 'static,
    FR: Future<Output = Result<R, E>> + Send + 'static,
{
    let mbid = match_artist_url(url).expect("Invalid MusicBrainz artist URL");
    client
        .get::<T, _, _, _, _>(
            &format!("artist/{mbid}"),
            &[("inc", ARTIST_PARTS)],
            |value| callback(value, mbid),
        )
        .await
}

pub async fn get_artist_release_groups_raw<T, F, R, E, FR>(
    client: &MusicBrainzClient,
    url: &str,
    callback: F,
) -> Result<R, E>
where
    T: std::any::Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
    F: FnOnce(&T) -> FR,
    E: From<Error> + Send + 'static,
    FR: Future<Output = Result<R, E>> + Send + 'static,
{
    let mbid = match_artist_url(url).expect("Invalid MusicBrainz artist URL");
    let limit_str = RELEASE_GROUPS_LIMIT.to_string();
    client
        .get::<T, _, _, _, _>(
            "release-group",
            &[
                ("artist", mbid),
                ("limit", limit_str.as_str()),
                ("offset", "0"),
            ],
            |value| callback(value),
        )
        .await
}

pub async fn get_artist_releases_raw<T, F, R, E, FR>(
    client: &MusicBrainzClient,
    url: &str,
    callback: F,
) -> Result<R, E>
where
    T: std::any::Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
    F: FnOnce(&T) -> FR,
    E: From<Error> + Send + 'static,
    FR: Future<Output = Result<R, E>> + Send + 'static,
{
    let mbid = match_artist_url(url).expect("Invalid MusicBrainz artist URL");
    let limit_str = RELEASES_LIMIT.to_string();
    client
        .get::<T, _, _, _, _>(
            "release",
            &[
                ("artist", mbid),
                ("limit", limit_str.as_str()),
                ("offset", "0"),
            ],
            |value| callback(value),
        )
        .await
}

pub async fn get_artist_recordings_raw<T, F, R, E, FR>(
    client: &MusicBrainzClient,
    url: &str,
    callback: F,
) -> Result<R, E>
where
    T: std::any::Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
    F: FnOnce(&T) -> FR,
    E: From<Error> + Send + 'static,
    FR: Future<Output = Result<R, E>> + Send + 'static,
{
    let mbid = match_artist_url(url).expect("Invalid MusicBrainz artist URL");
    let limit_str = RECORDINGS_LIMIT.to_string();
    client
        .get::<T, _, _, _, _>(
            "recording",
            &[
                ("artist", mbid),
                ("limit", limit_str.as_str()),
                ("offset", "0"),
            ],
            |value| callback(value),
        )
        .await
}

pub async fn get_artist(client: &MusicBrainzClient, url: &str) -> Result<EntityResult<()>, Error> {
    get_artist_raw::<ArtistResponse, _, _, Error, _>(client, url, |a, mbid| {
        let artist_url = artist_url(mbid);

        // Primary alias from the canonical name.
        let mut aliases = vec![Alias {
            name: a.name.clone(),
            source: SOURCE.into(),
            primary: true,
            ..Default::default()
        }];
        // Aliases from the API.
        for alias in &a.aliases {
            if let Some(name) = &alias.name {
                aliases.push(Alias {
                    name: name.clone(),
                    source: SOURCE.into(),
                    primary: match &alias.primary {
                        Some(serde_json::Value::String(s)) => s == "primary",
                        Some(serde_json::Value::Bool(b)) => *b,
                        _ => false,
                    },
                    ..Default::default()
                });
            }
        }

        // Source 0: release-groups (ReleaseGroup children)
        let rg_source = PaginatedChildSource::new(Box::new(ArtistReleaseGroupsPageFetcher {
            client: client.clone(),
            artist_id: mbid.to_string(),
        }))
        .with_static_eval(release_group_eval);

        // Source 1: releases (Release children — standalone releases not in a release-group)
        let release_source = PaginatedChildSource::new(Box::new(ArtistReleasesPageFetcher {
            client: client.clone(),
            artist_id: mbid.to_string(),
        }))
        .with_static_eval(release_eval);

        // Source 2: recordings (Track children — standalone recordings)
        let recording_source = PaginatedChildSource::new(Box::new(ArtistRecordingsPageFetcher {
            client: client.clone(),
            artist_id: mbid.to_string(),
        }))
        .with_static_eval(recording_eval);

        // Source 3: recording relationships (appearances — recordings linked via relationships,
        // not artist credit; e.g. guest performances, session work).
        let appearance_refs: Vec<ChildRef> = a
            .relations
            .iter()
            .filter(|r| r.target_type == "recording")
            .filter_map(|r| r.recording.as_ref())
            .map(|rec| ChildRef {
                entry_type: EntryType::Track,
                external_type: EXTERNAL_TYPE_RECORDING.into(),
                sources: [(SOURCE.into(), HashSet::from([recording_url(&rec.id)]))].into(),
                name: rec.title.clone(),
                ..Default::default()
            })
            .collect();

        let mut sources: crate::providers::types::ExternalSources =
            [(SOURCE.into(), HashSet::from([artist_url]))].into();
        for rel in a.relations.iter().filter(|r| r.target_type == "url") {
            if let Some(u) = &rel.url {
                let key = url_source_key(&u.resource);
                sources
                    .0
                    .entry(key.into())
                    .or_default()
                    .insert(u.resource.clone());
            }
        }

        let result = Ok(EntityResult {
            release_date: None,
            sources,
            extra: a.extra.clone(),
            specific_data: EntrySpecificData::Artist,
            children: vec![
                Arc::new(CachedChildSource::new(Box::new(rg_source))),
                Arc::new(CachedChildSource::new(Box::new(release_source))),
                Arc::new(CachedChildSource::new(Box::new(recording_source))),
                Arc::new(CachedChildSource::from_children(appearance_refs)),
            ],
            aliases,
        });
        async move { result }
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, sync::Arc};

    use http::Method;

    use crate::{
        providers::{
            backends::musicbrainz::{
                SOURCE,
                artist::{
                    ARTIST_PARTS, ArtistRecordingsResponse, ArtistReleaseGroupsResponse,
                    ArtistReleasesResponse, ArtistResponse, RECORDINGS_LIMIT, RELEASE_GROUPS_LIMIT,
                    RELEASES_LIMIT, get_artist,
                },
                client::MusicBrainzClient,
                types::{
                    EXTERNAL_TYPE_RECORDING, EXTERNAL_TYPE_RELEASE, EXTERNAL_TYPE_RELEASE_GROUP,
                },
            },
            types::{ChildSource, EntrySpecificData, EntryType},
        },
        test_utils::MockHttpClient,
    };

    fn artist_api_url(mbid: &str) -> String {
        MusicBrainzClient::build_url(&format!("artist/{mbid}"), &[("inc", ARTIST_PARTS)])
    }

    fn release_groups_browse_url(artist_mbid: &str) -> String {
        let limit = RELEASE_GROUPS_LIMIT.to_string();
        MusicBrainzClient::build_url(
            "release-group",
            &[("artist", artist_mbid), ("limit", &limit), ("offset", "0")],
        )
    }

    fn releases_browse_url(artist_mbid: &str) -> String {
        let limit = RELEASES_LIMIT.to_string();
        MusicBrainzClient::build_url(
            "release",
            &[("artist", artist_mbid), ("limit", &limit), ("offset", "0")],
        )
    }

    fn recordings_browse_url(artist_mbid: &str) -> String {
        let limit = RECORDINGS_LIMIT.to_string();
        MusicBrainzClient::build_url(
            "recording",
            &[("artist", artist_mbid), ("limit", &limit), ("offset", "0")],
        )
    }

    #[tokio::test]
    async fn test_get_artist_with_children() -> anyhow::Result<()> {
        let mbid = "201500bb-d0b7-49bf-9869-50e6496350b8";

        let mut http_client = MockHttpClient::new();
        http_client.add_route_json::<ArtistResponse>(
            Method::GET,
            &artist_api_url(mbid),
            include_str!("./artist_watame.json"),
        );
        http_client.add_route_json::<ArtistReleaseGroupsResponse>(
            Method::GET,
            &release_groups_browse_url(mbid),
            include_str!("./artist_watame_release_groups.json"),
        );
        http_client.add_route_json::<ArtistReleasesResponse>(
            Method::GET,
            &releases_browse_url(mbid),
            include_str!("./artist_watame_releases.json"),
        );
        http_client.add_route_json::<ArtistRecordingsResponse>(
            Method::GET,
            &recordings_browse_url(mbid),
            include_str!("./artist_watame_recordings.json"),
        );

        let client = MusicBrainzClient::new_with_client(Arc::new(http_client), None)?;
        let artist = get_artist(&client, &format!("https://musicbrainz.org/artist/{mbid}")).await?;

        // Metadata
        assert!(matches!(artist.specific_data, EntrySpecificData::Artist));
        assert_eq!(
            artist.sources.get(SOURCE).unwrap(),
            &HashSet::from([format!("https://musicbrainz.org/artist/{mbid}")])
        );

        // Aliases: canonical name + 2 from API
        assert!(
            artist
                .aliases
                .iter()
                .any(|a| a.name == "角巻わため" && a.primary)
        );
        assert!(artist.aliases.iter().any(|a| a.name == "Tsunomaki Watame"));

        // Four child sources: release-groups, releases, recordings, appearances (recording-rels)
        assert_eq!(artist.children.len(), 4);

        // Source 0: release-groups — 50 items, no next page
        let mut rg_cursor = artist.children[0].cursor();
        let (first_rg, _) = rg_cursor.next().await?.expect("expected release-group");
        assert_eq!(first_rg.entry_type, EntryType::ReleaseGroup);
        assert_eq!(first_rg.external_type.as_ref(), EXTERNAL_TYPE_RELEASE_GROUP);
        assert_eq!(first_rg.name.as_deref(), Some("sweet night, sweet time..."));
        assert_eq!(
            first_rg.sources.get(SOURCE).unwrap(),
            &HashSet::from([
                "https://musicbrainz.org/release-group/0167b61f-f592-453d-90b0-3c33975f60aa"
                    .to_string()
            ])
        );

        // Source 1: releases — 62 items (all fit in one page)
        let mut rel_cursor = artist.children[1].cursor();
        let (first_rel, _) = rel_cursor.next().await?.expect("expected release");
        assert_eq!(first_rel.entry_type, EntryType::Release);
        assert_eq!(first_rel.external_type.as_ref(), EXTERNAL_TYPE_RELEASE);
        assert_eq!(first_rel.name.as_deref(), Some("FAKE LAND"));

        // Source 2: recordings — 123 total, first page returns 100
        // Only read the first item to avoid fetching page 2 (no mock registered)
        let mut rec_cursor = artist.children[2].cursor();
        let (first_rec, _) = rec_cursor.next().await?.expect("expected recording");
        assert_eq!(first_rec.entry_type, EntryType::Track);
        assert_eq!(first_rec.external_type.as_ref(), EXTERNAL_TYPE_RECORDING);
        assert_eq!(first_rec.name.as_deref(), Some("Surges"));

        // Source 3: appearances via recording relationships — 70 items (embedded, no pagination)
        let mut app_cursor = artist.children[3].cursor();
        let (first_app, _) = app_cursor.next().await?.expect("expected appearance");
        assert_eq!(first_app.entry_type, EntryType::Track);
        assert_eq!(first_app.external_type.as_ref(), EXTERNAL_TYPE_RECORDING);
        assert_eq!(first_app.name.as_deref(), Some("Everlasting Soul"));
        assert_eq!(
            first_app.sources.get(SOURCE).unwrap(),
            &HashSet::from([
                "https://musicbrainz.org/recording/976fb404-b5bf-47be-a391-403718809281"
                    .to_string()
            ])
        );

        Ok(())
    }
}
