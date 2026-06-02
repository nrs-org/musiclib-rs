use std::{collections::HashSet, future::Future, sync::Arc};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::providers::{
    backends::spotify::{
        SOURCE,
        canonicalize::{album_url, artist_url, match_artist_url},
        client::SpotifyClient,
        types::{EXTERNAL_TYPE_ALBUM, Image, Paging},
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
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub images: Vec<Image>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimplifiedAlbum {
    pub id: String,
    pub name: String,
    pub album_type: Option<String>,
    pub release_date: Option<String>,
    pub release_date_precision: Option<String>,
    pub total_tracks: Option<u32>,
}

// --- Constants ---

pub(crate) const ARTIST_ALBUMS_LIMIT: u32 = 50;
const ARTIST_ALBUMS_INCLUDE_GROUPS: &str = "album,single,compilation,appears_on";

// --- Page fetcher ---

struct ArtistAlbumsPageFetcher {
    client: SpotifyClient,
    artist_id: String,
}

#[async_trait::async_trait]
impl PageFetcher for ArtistAlbumsPageFetcher {
    async fn fetch_page(&mut self, page_token: Option<&str>) -> Result<ChildPage, Error> {
        let offset: u32 = page_token.and_then(|t| t.parse().ok()).unwrap_or(0);
        let limit_str = ARTIST_ALBUMS_LIMIT.to_string();
        let offset_str = offset.to_string();

        let response = self
            .client
            .get::<Paging<SimplifiedAlbum>, _, _, Error, _>(
                &format!("artists/{}/albums", self.artist_id),
                &[
                    ("limit", limit_str.as_str()),
                    ("offset", offset_str.as_str()),
                    ("include_groups", ARTIST_ALBUMS_INCLUDE_GROUPS),
                ],
                |r| {
                    let r = r.clone();
                    async move { Ok(r) }
                },
            )
            .await?;

        let fetched = response.items.len() as u32;
        let next_offset = offset + fetched;
        let next_page_token = if next_offset < response.total {
            Some(next_offset.to_string())
        } else {
            None
        };

        let children = response
            .items
            .into_iter()
            .map(|album| ChildRef {
                entry_type: EntryType::Release,
                external_type: EXTERNAL_TYPE_ALBUM.into(),
                sources: [(SOURCE.into(), HashSet::from([album_url(&album.id)]))].into(),
                name: Some(album.name),
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

fn album_eval(expr: &CompiledMatcherExpr) -> Tribool {
    static_eval_expr(expr, &|matcher| match matcher {
        CompiledChildMatcher::EntryData(CompiledEntryDataMatcher::EntryType(t)) => {
            (*t == EntryType::Release).into()
        }
        CompiledChildMatcher::EntryData(CompiledEntryDataMatcher::ExternalType(t)) => {
            (t == EXTERNAL_TYPE_ALBUM).into()
        }
        _ => default_eval_leaf(matcher),
    })
}

// --- Public API ---

pub async fn get_artist_raw<T, F, R, E, FR>(
    client: &SpotifyClient,
    url: &str,
    callback: F,
) -> Result<R, E>
where
    T: std::any::Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
    F: FnOnce(&T, &str) -> FR,
    E: From<Error> + Send + 'static,
    FR: Future<Output = Result<R, E>> + Send + 'static,
{
    let id = match_artist_url(url).expect("Invalid Spotify artist URL");
    client
        .get::<T, _, _, _, _>(&format!("artists/{id}"), &[], |value| callback(value, id))
        .await
}

pub async fn get_artist_albums_raw<T, F, R, E, FR>(
    client: &SpotifyClient,
    url: &str,
    callback: F,
) -> Result<R, E>
where
    T: std::any::Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
    F: FnOnce(&T) -> FR,
    E: From<Error> + Send + 'static,
    FR: Future<Output = Result<R, E>> + Send + 'static,
{
    let id = match_artist_url(url).expect("Invalid Spotify artist URL");
    let limit_str = ARTIST_ALBUMS_LIMIT.to_string();
    client
        .get::<T, _, _, _, _>(
            &format!("artists/{id}/albums"),
            &[
                ("limit", limit_str.as_str()),
                ("offset", "0"),
                ("include_groups", ARTIST_ALBUMS_INCLUDE_GROUPS),
            ],
            |value| callback(value),
        )
        .await
}

pub async fn get_artist(client: &SpotifyClient, url: &str) -> Result<EntityResult<()>, Error> {
    get_artist_raw::<ArtistResponse, _, _, Error, _>(client, url, |a, id| {
        let canonical = artist_url(id);

        let albums_source = PaginatedChildSource::new(Box::new(ArtistAlbumsPageFetcher {
            client: client.clone(),
            artist_id: id.to_string(),
        }))
        .with_static_eval(album_eval);

        let result = Ok(EntityResult {
            release_date: None,
            sources: [(SOURCE.into(), HashSet::from([canonical]))].into(),
            extra: a.extra.clone(),
            specific_data: EntrySpecificData::Artist,
            children: vec![Arc::new(CachedChildSource::new(Box::new(albums_source)))],
            aliases: vec![Alias {
                name: a.name.clone(),
                source: SOURCE.into(),
                primary: true,
                ..Default::default()
            }],
        });
        async move { result }
    })
    .await
}

#[cfg(test)]
mod tests {
    use crate::providers::types::child_next;
    use std::sync::Arc;

    use bytes::Bytes;

    use crate::{
        http::{Method, ResponseStatus},
        providers::{
            backends::spotify::{
                SOURCE,
                artist::{
                    ARTIST_ALBUMS_INCLUDE_GROUPS, ARTIST_ALBUMS_LIMIT, ArtistResponse, get_artist,
                },
                canonicalize::artist_url,
                client::SpotifyClient,
                types::{EXTERNAL_TYPE_ALBUM, Paging},
            },
            types::{ChildSource, EntrySpecificData, EntryType},
        },
        test_utils::MockHttpClient,
    };

    use super::SimplifiedAlbum;

    fn artist_api_url(id: &str) -> String {
        SpotifyClient::build_url(&format!("artists/{id}"), &[])
    }

    fn artist_albums_url(id: &str) -> String {
        let limit = ARTIST_ALBUMS_LIMIT.to_string();
        SpotifyClient::build_url(
            &format!("artists/{id}/albums"),
            &[
                ("limit", &limit),
                ("offset", "0"),
                ("include_groups", ARTIST_ALBUMS_INCLUDE_GROUPS),
            ],
        )
    }

    #[tokio::test]
    async fn test_get_artist() -> anyhow::Result<()> {
        let id = "68609MOnEU86kVyMf26JnM";
        let mut http_client = MockHttpClient::new();
        http_client.add_route_json::<ArtistResponse>(
            Method::GET,
            &artist_api_url(id),
            include_str!("./artist_watame.json"),
        );
        http_client.add_route_json::<Paging<SimplifiedAlbum>>(
            Method::GET,
            &artist_albums_url(id),
            include_str!("./artist_watame_albums.json"),
        );

        let client = SpotifyClient::new_with_token(Arc::new(http_client), "test_token");
        let artist = get_artist(&client, &artist_url(id)).await?;

        assert!(matches!(artist.specific_data, EntrySpecificData::Artist));
        assert_eq!(artist.aliases.len(), 1);
        assert_eq!(artist.aliases[0].name, "角巻わため");
        assert!(artist.aliases[0].primary);
        assert_eq!(
            artist.sources.get(SOURCE).unwrap(),
            &std::collections::HashSet::from([format!("https://open.spotify.com/artist/{id}")])
        );

        // One child source: albums (22 total, first page of 50 fits all)
        assert_eq!(artist.children.len(), 1);
        let mut albums = artist.children[0].cursor();

        // First album: Hop Step Sheep
        let (first_album, _) = child_next(&mut albums)
            .await?
            .expect("expected first album");
        assert_eq!(first_album.entry_type, EntryType::Release);
        assert_eq!(first_album.external_type.as_ref(), EXTERNAL_TYPE_ALBUM);
        assert_eq!(first_album.name.as_deref(), Some("Hop Step Sheep"));
        assert_eq!(
            first_album.sources.get(SOURCE).unwrap(),
            &std::collections::HashSet::from([
                "https://open.spotify.com/album/0LCmjkgN0CqcPtLuNHUBma".to_string()
            ])
        );

        // Second album: わためのうた vol.２
        let (second_album, _) = child_next(&mut albums)
            .await?
            .expect("expected second album");
        assert_eq!(second_album.name.as_deref(), Some("わためのうた vol.２"));

        Ok(())
    }

    #[tokio::test]
    async fn test_get_artist_token_expiry() -> anyhow::Result<()> {
        const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
        let id = "68609MOnEU86kVyMf26JnM";
        let mut http_client = MockHttpClient::new();

        // First call to the artist endpoint returns 401 (expired token).
        http_client.add_route(
            Method::GET,
            &artist_api_url(id),
            ResponseStatus::UNAUTHORIZED,
            Bytes::from_static(br#"{"error":{"status":401,"message":"No token provided"}}"#),
        );
        // Second call (after token refresh) returns the real data.
        http_client.add_route_json::<ArtistResponse>(
            Method::GET,
            &artist_api_url(id),
            include_str!("./artist_watame.json"),
        );
        http_client.add_route_json::<Paging<SimplifiedAlbum>>(
            Method::GET,
            &artist_albums_url(id),
            include_str!("./artist_watame_albums.json"),
        );
        // Token refresh endpoint.
        http_client.add_route(
            Method::POST,
            TOKEN_URL,
            ResponseStatus::OK,
            Bytes::from_static(br#"{"access_token":"fresh_token"}"#),
        );

        let client =
            SpotifyClient::new_with_client(Arc::new(http_client), "client_id", "client_secret");
        let artist = get_artist(&client, &artist_url(id)).await?;

        assert_eq!(artist.aliases[0].name, "角巻わため");

        Ok(())
    }
}
