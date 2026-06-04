use std::{collections::HashSet, future::Future, sync::Arc};

use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;

use crate::providers::{
    backends::{
        soundcloud::{
            SOURCE,
            canonicalize::{artist_albums_url, artist_sets_url},
        },
        ytdlp::YtdlpClient,
    },
    types::{
        Alias, CachedChildSource, ChildRef, EntityResult, EntrySpecificData, EntryType, Error,
        ExternalSources,
    },
};

use super::{EXTERNAL_TYPE_PLAYLIST, EXTERNAL_TYPE_TRACK};

#[derive(Clone, Deserialize)]
pub struct ArtistResponse {
    pub webpage_url: String,
    pub title: String,
    #[serde(default)]
    pub entries: Vec<FlatTrackEntry>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Clone, Deserialize)]
pub struct SetsResponse {
    #[serde(default)]
    pub entries: Vec<FlatSetEntry>,
}

#[derive(Clone, Deserialize)]
pub struct FlatTrackEntry {
    pub url: String,
}

#[derive(Clone, Deserialize)]
pub struct FlatSetEntry {
    pub url: String,
    pub title: Option<String>,
}

pub async fn get_artist_raw<T, F, E, FR, R>(
    client: &YtdlpClient,
    url: &str,
    callback: F,
) -> Result<R, E>
where
    T: DeserializeOwned + Send + 'static,
    F: FnOnce(&T) -> FR + Send,
    E: From<Error> + Send + 'static,
    FR: Future<Output = Result<R, E>> + Send + 'static,
    R: Send,
{
    let value: T = client.fetch_as(url).await?;
    callback(&value).await
}

pub async fn get_artist_sets_raw<T, F, E, FR, R>(
    client: &YtdlpClient,
    artist_url: &str,
    callback: F,
) -> Result<R, E>
where
    T: DeserializeOwned + Send + 'static,
    F: FnOnce(&T) -> FR + Send,
    E: From<Error> + Send + 'static,
    FR: Future<Output = Result<R, E>> + Send + 'static,
    R: Send,
{
    let value: T = client.fetch_as(&artist_sets_url(artist_url)).await?;
    callback(&value).await
}

pub async fn get_artist_albums_raw<T, F, E, FR, R>(
    client: &YtdlpClient,
    artist_url: &str,
    callback: F,
) -> Result<R, E>
where
    T: DeserializeOwned + Send + 'static,
    F: FnOnce(&T) -> FR + Send,
    E: From<Error> + Send + 'static,
    FR: Future<Output = Result<R, E>> + Send + 'static,
    R: Send,
{
    let value: T = client.fetch_as(&artist_albums_url(artist_url)).await?;
    callback(&value).await
}

fn sets_value_to_child_refs(value: Value) -> Result<Vec<ChildRef>, Error> {
    let sets: SetsResponse = serde_json::from_value(value)
        .map_err(|e| Error::InvalidUrl(format!("failed to deserialize sets: {e}")))?;
    Ok(sets
        .entries
        .into_iter()
        .map(|entry| ChildRef {
            entry_type: EntryType::Release,
            external_type: EXTERNAL_TYPE_PLAYLIST.into(),
            sources: [(SOURCE.into(), HashSet::from([entry.url]))].into(),
            name: entry.title,
            ..Default::default()
        })
        .collect())
}

/// Fetches artist metadata without entries (fast) for the main URL, then returns
/// an `EntityResult` whose albums, sets, and track children are all lazy.
pub async fn get_artist(client: Arc<YtdlpClient>, url: &str) -> Result<EntityResult<()>, Error> {
    let artist: ArtistResponse = serde_json::from_value(client.fetch_no_children(url).await?)
        .map_err(|e| Error::InvalidUrl(format!("failed to deserialize artist metadata: {e}")))?;

    let mut sources = ExternalSources::default();
    sources
        .0
        .entry(SOURCE.into())
        .or_default()
        .insert(artist.webpage_url);

    let track_source = Arc::clone(&client).lazy_children(url.to_string(), |value| {
        let a: ArtistResponse = serde_json::from_value(value)
            .map_err(|e| Error::InvalidUrl(format!("failed to deserialize artist tracks: {e}")))?;
        Ok(a.entries
            .into_iter()
            .map(|entry| ChildRef {
                entry_type: EntryType::Track,
                external_type: EXTERNAL_TYPE_TRACK.into(),
                sources: [(SOURCE.into(), HashSet::from([entry.url]))].into(),
                ..Default::default()
            })
            .collect())
    });

    let albums_source =
        Arc::clone(&client).lazy_children(artist_albums_url(url), sets_value_to_child_refs);
    let sets_source =
        Arc::clone(&client).lazy_children(artist_sets_url(url), sets_value_to_child_refs);

    Ok(EntityResult {
        release_date: None,
        sources,
        extra: artist.extra,
        specific_data: EntrySpecificData::Artist,
        children: vec![
            Arc::new(CachedChildSource::new(Box::new(albums_source))),
            Arc::new(CachedChildSource::new(Box::new(sets_source))),
            Arc::new(CachedChildSource::new(Box::new(track_source))),
        ],
        aliases: vec![Alias {
            name: artist.title,
            source: SOURCE.into(),
            primary: true,
            ..Default::default()
        }],
    })
}

#[cfg(test)]
mod tests {
    use crate::providers::types::child_next;
    use std::sync::Arc;

    use crate::{
        http::Method,
        providers::{
            backends::{
                soundcloud::{
                    SOURCE,
                    canonicalize::{artist_albums_url, artist_sets_url},
                },
                ytdlp::YtdlpClient,
            },
            types::{EntrySpecificData, EntryType},
        },
        test_utils::MockHttpClient,
    };

    use super::get_artist;

    const SERVER: &str = "http://mock";

    fn entity_url(url: &str) -> String {
        format!("{}/entities/{}", SERVER, urlencoding::encode(url))
    }

    fn make_artist_client(url: &str) -> Arc<YtdlpClient> {
        let mut http = MockHttpClient::new();
        // metadata-only fetch for the main artist URL
        http.add_route_json::<serde_json::Value>(
            Method::GET,
            &format!("{}?no_entries", entity_url(url)),
            include_str!("./artist_laserimouto.json"),
        );
        // lazy track fetch (full)
        http.add_route_json::<serde_json::Value>(
            Method::GET,
            &entity_url(url),
            include_str!("./artist_laserimouto.json"),
        );
        // lazy sets/albums fetches (always full)
        http.add_route_json::<serde_json::Value>(
            Method::GET,
            &entity_url(&artist_sets_url(url)),
            include_str!("./artist_laserimouto_sets.json"),
        );
        http.add_route_json::<serde_json::Value>(
            Method::GET,
            &entity_url(&artist_albums_url(url)),
            include_str!("./artist_laserimouto_albums.json"),
        );
        Arc::new(YtdlpClient::new(Arc::new(http), SERVER.to_string()))
    }

    #[tokio::test]
    async fn test_get_artist() -> anyhow::Result<()> {
        let url = "https://soundcloud.com/laserimouto";
        let client = make_artist_client(url);
        let artist = get_artist(client, url).await?;

        assert_eq!(artist.aliases.len(), 1);
        assert_eq!(artist.aliases[0].name, "Laser Imouto (All)");
        assert!(artist.aliases[0].primary);

        assert!(matches!(artist.specific_data, EntrySpecificData::Artist));

        assert_eq!(
            artist.sources.get(SOURCE).unwrap(),
            &std::collections::HashSet::from([url.to_string()])
        );

        // Three child sources: albums, sets, tracks
        assert_eq!(artist.children.len(), 3);

        // albums (empty for laserimouto)
        let mut albums = artist.children[0].cursor();
        assert!(child_next(&mut albums).await?.is_none());

        // sets (3 for laserimouto)
        let mut sets = artist.children[1].cursor();
        let (first_set, _) = child_next(&mut sets).await?.expect("expected first set");
        assert_eq!(first_set.entry_type, EntryType::Release);
        assert_eq!(first_set.name.as_deref(), Some("Compilation Works"));
        assert_eq!(
            first_set.sources.get(SOURCE).unwrap(),
            &std::collections::HashSet::from([
                "https://soundcloud.com/laserimouto/sets/compilation-works".to_string()
            ])
        );

        // tracks (32 for laserimouto)
        let mut tracks = artist.children[2].cursor();
        let (first_track, _) = child_next(&mut tracks)
            .await?
            .expect("expected first track");
        assert_eq!(first_track.entry_type, EntryType::Track);

        Ok(())
    }
}
