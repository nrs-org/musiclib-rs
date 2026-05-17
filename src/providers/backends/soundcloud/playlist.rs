use std::{collections::HashSet, future::Future, sync::Arc};

use serde::{Deserialize, de::DeserializeOwned};

use crate::providers::{
    backends::{soundcloud::SOURCE, ytdlp::YtdlpClient},
    std_values::StandardRoleNames,
    types::{
        Alias, CachedChildSource, ChildRef, Contribution, EntityResult, EntrySpecificData,
        EntryType, Error, ExternalSources, TrackPosition,
    },
};

use super::{EXTERNAL_TYPE_ARTIST, EXTERNAL_TYPE_TRACK};

#[derive(Clone, Deserialize)]
pub struct PlaylistResponse {
    pub webpage_url: String,
    pub title: String,
    pub uploader: Option<String>,
    pub playlist_count: Option<i32>,
    #[serde(default)]
    pub entries: Vec<FlatEntry>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Clone, Deserialize)]
pub struct FlatEntry {
    pub url: String,
}

impl PlaylistResponse {
    pub fn into_entity_result(self) -> EntityResult<()> {
        let mut sources = ExternalSources::default();
        sources
            .0
            .entry(SOURCE.into())
            .or_default()
            .insert(self.webpage_url);

        let uploader_ref = ChildRef {
            entry_type: EntryType::Artist,
            external_type: EXTERNAL_TYPE_ARTIST.into(),
            sources: ExternalSources::default(),
            name: self.uploader,
            contributions: vec![Contribution {
                role: StandardRoleNames::UPLOADER.into(),
                main_artist: false,
                source: SOURCE.into(),
                extra: serde_json::Value::Null,
            }],
            ..Default::default()
        };

        let track_refs: Vec<ChildRef> = self
            .entries
            .into_iter()
            .enumerate()
            .map(|(i, entry)| ChildRef {
                entry_type: EntryType::Track,
                external_type: EXTERNAL_TYPE_TRACK.into(),
                sources: [(SOURCE.into(), HashSet::from([entry.url]))].into(),
                position: Some(TrackPosition {
                    disc_no: None,
                    track_no: (i + 1) as i32,
                }),
                ..Default::default()
            })
            .collect();

        EntityResult {
            release_date: None,
            sources,
            extra: self.extra,
            specific_data: EntrySpecificData::Release {
                release_type: Some("playlist".into()),
                num_discs: None,
                num_tracks: self.playlist_count,
            },
            children: vec![
                Arc::new(CachedChildSource::from_children(vec![uploader_ref])),
                Arc::new(CachedChildSource::from_children(track_refs)),
            ],
            aliases: vec![Alias {
                name: self.title,
                source: SOURCE.into(),
                primary: true,
                ..Default::default()
            }],
        }
    }
}

pub async fn get_playlist_raw<T, F, E, FR, R>(
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

pub async fn get_playlist(client: &YtdlpClient, url: &str) -> Result<EntityResult<()>, Error> {
    get_playlist_raw::<PlaylistResponse, _, Error, _, _>(client, url, |p| {
        let result = Ok(p.clone().into_entity_result());
        async move { result }
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{
        http::Method,
        providers::{
            backends::{soundcloud::SOURCE, ytdlp::YtdlpClient},
            types::{ChildSource, EntrySpecificData, EntryType},
        },
        test_utils::MockHttpClient,
    };

    use super::get_playlist;

    const SERVER: &str = "http://mock";

    fn entity_url(url: &str) -> String {
        format!("{}/entities/{}", SERVER, urlencoding::encode(url))
    }

    fn make_client(url: &str, fixture: &'static str) -> YtdlpClient {
        let mut http = MockHttpClient::new();
        http.add_route_json::<serde_json::Value>(Method::GET, &entity_url(url), fixture);
        YtdlpClient::new(Arc::new(http), SERVER.to_string())
    }

    #[tokio::test]
    async fn test_get_playlist() -> anyhow::Result<()> {
        let url = "https://soundcloud.com/laserimouto/sets/anime-hardcore-bootleg";
        let client = make_client(url, include_str!("./playlist_anime_hardcore_bootleg.json"));
        let playlist = get_playlist(&client, url).await?;

        assert_eq!(playlist.aliases.len(), 1);
        assert_eq!(playlist.aliases[0].name, "Otaku Hardcore Bootlegs");
        assert!(playlist.aliases[0].primary);

        assert!(matches!(
            playlist.specific_data,
            EntrySpecificData::Release {
                num_tracks: Some(14),
                ..
            }
        ));

        assert_eq!(
            playlist.sources.get(SOURCE).unwrap(),
            &std::collections::HashSet::from([url.to_string()])
        );

        // Two child sources: uploader artist + tracks
        assert_eq!(playlist.children.len(), 2);

        let mut uploaders = playlist.children[0].cursor();
        let (uploader, _) = uploaders.next().await?.expect("expected uploader");
        assert_eq!(uploader.entry_type, EntryType::Artist);
        assert_eq!(uploader.name.as_deref(), Some("Laser Imouto"));

        let mut tracks = playlist.children[1].cursor();
        let (first_track, _) = tracks.next().await?.expect("expected first track");
        assert_eq!(first_track.entry_type, EntryType::Track);
        assert_eq!(first_track.position.as_ref().map(|p| p.track_no), Some(1));
        assert_eq!(
            first_track.sources.get(SOURCE).unwrap(),
            &std::collections::HashSet::from([
                "https://soundcloud.com/laserimouto/city-of-gold".to_string()
            ])
        );

        Ok(())
    }
}
