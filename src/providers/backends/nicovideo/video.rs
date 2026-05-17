use std::{future::Future, sync::Arc};

use serde::{Deserialize, de::DeserializeOwned};

use crate::providers::{
    backends::{nicovideo::SOURCE, ytdlp::YtdlpClient},
    std_values::StandardRoleNames,
    types::{
        Alias, CachedChildSource, ChildRef, Contribution, EntityResult, EntrySpecificData,
        EntryType, Error, ExternalSources,
    },
};

use super::{EXTERNAL_TYPE_ARTIST, canonicalize::user_url};

#[derive(Clone, Deserialize)]
pub struct VideoResponse {
    pub webpage_url: String,
    pub title: String,
    pub duration: Option<f64>,
    pub upload_date: Option<String>,
    pub uploader: Option<String>,
    pub uploader_id: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

fn upload_date_to_release_date(date: &str) -> Option<String> {
    if date.len() == 8 {
        Some(format!("{}-{}-{}", &date[..4], &date[4..6], &date[6..8]))
    } else {
        None
    }
}

impl VideoResponse {
    pub fn into_entity_result(self) -> EntityResult<()> {
        let mut sources = ExternalSources::default();
        sources
            .0
            .entry(SOURCE.into())
            .or_default()
            .insert(self.webpage_url);

        let mut artist_sources = ExternalSources::default();
        if let Some(ref uid) = self.uploader_id {
            artist_sources
                .0
                .entry(SOURCE.into())
                .or_default()
                .insert(user_url(uid));
        }
        let artist_ref = ChildRef {
            entry_type: EntryType::Artist,
            external_type: EXTERNAL_TYPE_ARTIST.into(),
            sources: artist_sources,
            name: self.uploader,
            contributions: vec![Contribution {
                role: StandardRoleNames::LISTED_ARTIST.into(),
                main_artist: true,
                source: SOURCE.into(),
                extra: serde_json::Value::Null,
            }],
            ..Default::default()
        };

        EntityResult {
            release_date: self
                .upload_date
                .as_deref()
                .and_then(upload_date_to_release_date),
            sources,
            extra: self.extra,
            specific_data: EntrySpecificData::Track {
                duration_ms: self.duration.map(|d| (d * 1000.0) as i64),
                positions: Default::default(),
            },
            children: vec![Arc::new(CachedChildSource::from_children(vec![artist_ref]))],
            aliases: vec![Alias {
                name: self.title,
                source: SOURCE.into(),
                primary: true,
                ..Default::default()
            }],
        }
    }
}

pub async fn get_video_raw<T, F, E, FR, R>(
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

pub async fn get_video(client: &YtdlpClient, url: &str) -> Result<EntityResult<()>, Error> {
    get_video_raw::<VideoResponse, _, Error, _, _>(client, url, |v| {
        let result = Ok(v.clone().into_entity_result());
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

    use super::get_video;

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
    async fn test_get_video() -> anyhow::Result<()> {
        let url = "https://www.nicovideo.jp/watch/sm46158033";
        let client = make_client(url, include_str!("./video_sm46158033.json"));
        let video = get_video(&client, url).await?;

        assert_eq!(video.aliases.len(), 1);
        assert!(video.aliases[0].primary);

        assert!(matches!(
            video.specific_data,
            EntrySpecificData::Track {
                duration_ms: Some(_),
                ..
            }
        ));

        assert_eq!(
            video.sources.get(SOURCE).unwrap(),
            &std::collections::HashSet::from([url.to_string()])
        );

        assert_eq!(video.children.len(), 1);
        let mut artists = video.children[0].cursor();
        let (artist, _) = artists.next().await?.expect("expected artist");
        assert_eq!(artist.entry_type, EntryType::Artist);
        assert_eq!(
            artist.sources.get(SOURCE).unwrap(),
            &std::collections::HashSet::from(
                ["https://www.nicovideo.jp/user/67047227".to_string()]
            )
        );

        Ok(())
    }
}
