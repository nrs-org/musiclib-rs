use std::{collections::HashSet, sync::Arc};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::future::Future;

use crate::providers::{
    backends::{
        ExtraJSON,
        youtube_api::{
            SOURCE,
            canonicalize::{ChannelKind, channel_url, match_channel_url},
            client::YoutubeClient,
        },
    },
    types::{Alias, CachedChildSource, EntityResult, EntrySpecificData, Error},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChannelListResponse {
    items: Vec<Channel>,
    #[serde(flatten)]
    extra: ExtraJSON,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Channel {
    snippet: ChannelSnippet,
    #[serde(flatten)]
    extra: ExtraJSON,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChannelSnippet {
    title: String,
    #[serde(rename = "customUrl")]
    custom_url: Option<String>,
    #[serde(flatten)]
    extra: ExtraJSON,
}

const PARTS: &str = "snippet";

fn channel_endpoint_and_params(
    channel_kind: ChannelKind,
    id: &str,
) -> (&'static str, [(&'static str, String); 2]) {
    let (key, value) = match channel_kind {
        ChannelKind::ChannelId => ("id", id.to_string()),
        ChannelKind::Custom | ChannelKind::User => ("forUsername", id.to_string()),
        ChannelKind::Handle => ("forHandle", id.trim_start_matches('@').to_string()),
    };
    ("channels", [("part", PARTS.to_string()), (key, value)])
}

pub async fn get_channel(client: &YoutubeClient, url: &str) -> Result<EntityResult<()>, Error> {
    let channel_match = match_channel_url(url).expect("Invalid YouTube channel URL");
    let url = channel_url(channel_match.kind, channel_match.id);
    let result = get_channel_raw::<ChannelListResponse, _, _, Error, _>(
        client,
        url.as_str(),
        move |c, kind, id| {
            let c = &c.items[0];
            let url = channel_url(kind, id);
            let mut source_set = HashSet::from([url.to_string()]);
            if let Some(custom_url) = &c.snippet.custom_url {
                source_set.insert(channel_url(ChannelKind::Custom, custom_url));
            }
            let result = Ok(EntityResult {
                release_date: None,
                sources: [(SOURCE.into(), source_set)].into(),
                extra: serde_json::to_value(c).unwrap_or_default(),
                specific_data: EntrySpecificData::Artist,
                children: Arc::new(CachedChildSource::from_children(Vec::new())),
                aliases: vec![Alias {
                    name: c.snippet.title.clone(),
                    source: SOURCE.into(),
                    primary: true,
                    ..Default::default()
                }],
            });
            async move { result }
        },
    )
    .await?;
    Ok(result)
}

pub async fn get_channel_raw<T, F, R, E, FR>(
    client: &YoutubeClient,
    url: &str,
    callback: F,
) -> Result<R, E>
where
    T: std::any::Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
    F: FnOnce(&T, ChannelKind, &str) -> FR,
    E: From<Error> + Send + 'static,
    FR: Future<Output = Result<R, E>> + Send + 'static,
{
    let channel_match = match_channel_url(url).expect("Invalid YouTube channel URL");
    let (endpoint, params) = channel_endpoint_and_params(channel_match.kind, channel_match.id);
    client
        .get::<T, _, _, _, _>(
            endpoint,
            &[
                (params[0].0, params[0].1.as_str()),
                (params[1].0, params[1].1.as_str()),
            ],
            |value| callback(value, channel_match.kind, channel_match.id),
        )
        .await
}
