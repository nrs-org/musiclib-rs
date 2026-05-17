use std::{collections::HashSet, future::Future, sync::Arc};

use serde::{Deserialize, de::DeserializeOwned};

use crate::providers::{
    backends::{nicovideo::SOURCE, ytdlp::YtdlpClient},
    std_values::StandardRoleNames,
    types::{
        Alias, CachedChildSource, ChildRef, Contribution, EntityResult, EntrySpecificData,
        EntryType, Error, ExternalSources, TrackPosition,
    },
};

use super::{EXTERNAL_TYPE_ARTIST, EXTERNAL_TYPE_VIDEO, canonicalize::user_url};

#[derive(Clone, Deserialize)]
pub struct SeriesResponse {
    pub webpage_url: String,
    pub title: Option<String>,
    pub uploader: Option<String>,
    pub uploader_id: Option<String>,
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

impl SeriesResponse {
    pub fn into_entity_result(self) -> EntityResult<()> {
        let mut sources = ExternalSources::default();
        sources
            .0
            .entry(SOURCE.into())
            .or_default()
            .insert(self.webpage_url);

        let mut uploader_sources = ExternalSources::default();
        if let Some(ref uid) = self.uploader_id {
            uploader_sources
                .0
                .entry(SOURCE.into())
                .or_default()
                .insert(user_url(uid));
        }
        let uploader_ref = ChildRef {
            entry_type: EntryType::Artist,
            external_type: EXTERNAL_TYPE_ARTIST.into(),
            sources: uploader_sources,
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
                external_type: EXTERNAL_TYPE_VIDEO.into(),
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
                release_type: Some("series".into()),
                num_discs: None,
                num_tracks: self.playlist_count,
            },
            children: vec![
                Arc::new(CachedChildSource::from_children(vec![uploader_ref])),
                Arc::new(CachedChildSource::from_children(track_refs)),
            ],
            aliases: self
                .title
                .map(|name| {
                    vec![Alias {
                        name,
                        source: SOURCE.into(),
                        primary: true,
                        ..Default::default()
                    }]
                })
                .unwrap_or_default(),
        }
    }
}

pub async fn get_series_raw<T, F, E, FR, R>(
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

pub async fn get_series(client: &YtdlpClient, url: &str) -> Result<EntityResult<()>, Error> {
    get_series_raw::<SeriesResponse, _, Error, _, _>(client, url, |s| {
        let result = Ok(s.clone().into_entity_result());
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
            backends::{nicovideo::SOURCE, ytdlp::YtdlpClient},
            types::{ChildSource, EntrySpecificData, EntryType},
        },
        test_utils::MockHttpClient,
    };

    use super::get_series;

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
    async fn test_get_series() -> anyhow::Result<()> {
        let url = "https://www.nicovideo.jp/series/396348";
        let client = make_client(url, include_str!("./series_396348.json"));
        let series = get_series(&client, url).await?;

        assert_eq!(series.aliases[0].name, "ヒカル＆店長シリーズ2018");
        assert!(series.aliases[0].primary);

        assert!(matches!(
            series.specific_data,
            EntrySpecificData::Release {
                num_tracks: Some(77),
                ..
            }
        ));

        assert_eq!(
            series.sources.get(SOURCE).unwrap(),
            &std::collections::HashSet::from([url.to_string()])
        );

        assert_eq!(series.children.len(), 2);

        let mut uploaders = series.children[0].cursor();
        let (uploader, _) = uploaders.next().await?.expect("expected uploader");
        assert_eq!(uploader.entry_type, EntryType::Artist);
        assert_eq!(
            uploader.sources.get(SOURCE).unwrap(),
            &std::collections::HashSet::from([
                "https://www.nicovideo.jp/user/123476264".to_string()
            ])
        );

        let mut tracks = series.children[1].cursor();
        let (first, _) = tracks.next().await?.expect("expected first track");
        assert_eq!(first.entry_type, EntryType::Track);
        assert_eq!(first.position.as_ref().map(|p| p.track_no), Some(1));

        Ok(())
    }
}
