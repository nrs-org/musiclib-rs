use std::{collections::HashSet, sync::Arc};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::future::Future;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::providers::{
    backends::{
        ExtraJSON,
        youtube_api::{
            SOURCE,
            canonicalize::{ChannelKind, channel_url, match_video_url, video_url},
            client::YoutubeClient,
            types::EXTERNAL_TYPE_CHANNEL_ID,
        },
    },
    std_values::StandardRoleNames,
    types::{
        Alias, CachedChildSource, ChildRef, Contribution, EntityResult, EntrySpecificData,
        EntryType, Error,
    },
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VideoListResponse {
    items: Vec<Video>,
    #[serde(flatten)]
    extra: ExtraJSON,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Video {
    snippet: VideoSnippet,
    #[serde(rename = "contentDetails")]
    content_details: VideoContentDetails,
    #[serde(flatten)]
    extra: ExtraJSON,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VideoSnippet {
    title: String,
    #[serde(rename = "publishedAt")]
    published_at: String,
    #[serde(rename = "channelId")]
    channel_id: String,
    #[serde(rename = "channelTitle")]
    channel_title: String,
    #[serde(flatten)]
    extra: ExtraJSON,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VideoContentDetails {
    duration: String,
    #[serde(flatten)]
    extra: ExtraJSON,
}

fn parse_iso_duration_ms(duration: &str) -> Option<i64> {
    let mut total_ms = 0;
    let mut num_buf = String::new();

    for c in duration.chars() {
        if c.is_ascii_digit() {
            num_buf.push(c);
        } else if !num_buf.is_empty() {
            let num: i64 = num_buf.parse().ok()?;
            match c {
                'H' => total_ms += num * 3600 * 1000,
                'M' => total_ms += num * 60 * 1000,
                'S' => total_ms += num * 1000,
                _ => return None,
            }
            num_buf.clear();
        }
    }

    Some(total_ms)
}

const PARTS: &str = "snippet,contentDetails";

pub async fn get_video_raw<T, F, R, E, FR>(
    client: &YoutubeClient,
    url: &str,
    callback: F,
) -> Result<R, E>
where
    T: std::any::Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
    F: FnOnce(&T, &str) -> FR,
    E: From<Error> + Send + 'static,
    FR: Future<Output = Result<R, E>> + Send + 'static,
{
    let id = match_video_url(url).expect("Invalid YouTube URL");
    client
        .get::<T, _, _, _, _>("videos", &[("part", PARTS), ("id", id)], |value| {
            callback(value, id)
        })
        .await
}

pub async fn get_video(client: &YoutubeClient, url: &str) -> Result<EntityResult<()>, Error> {
    let result = get_video_raw::<VideoListResponse, _, _, Error, _>(client, url, move |v, id| {
        let v = v.items.first().cloned();
        let id = id.to_string();
        async move {
            let id = id.as_str();
            let v = v.ok_or_else(|| Error::NotFound(format!("YouTube video not found: {id}")))?;
            let url = video_url(id);
            let release_date = OffsetDateTime::parse(&v.snippet.published_at, &Rfc3339)
                .ok()
                .map(|dt| {
                    format!(
                        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                        dt.year(),
                        dt.month() as u8,
                        dt.day(),
                        dt.hour(),
                        dt.minute(),
                        dt.second(),
                    )
                });
            let duration_ms = parse_iso_duration_ms(&v.content_details.duration);
            Ok(EntityResult {
                release_date,
                sources: [(SOURCE.into(), HashSet::from([url]))].into(),
                extra: serde_json::to_value(&v).unwrap_or_default(),
                specific_data: EntrySpecificData::Track {
                    duration_ms,
                    positions: Default::default(),
                },
                children: vec![Arc::new(CachedChildSource::from_children(vec![ChildRef {
                    entry_type: EntryType::Artist,
                    sources: [(
                        SOURCE.into(),
                        HashSet::from([channel_url(ChannelKind::ChannelId, &v.snippet.channel_id)]),
                    )]
                    .into(),
                    name: Some(v.snippet.channel_title.clone()),
                    external_type: EXTERNAL_TYPE_CHANNEL_ID.into(),
                    contributions: vec![Contribution {
                        role: StandardRoleNames::UPLOADER.into(),
                        main_artist: true,
                        source: SOURCE.into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                }]))],
                aliases: vec![Alias {
                    name: v.snippet.title.clone(),
                    source: SOURCE.into(),
                    primary: true,
                    ..Default::default()
                }],
            })
        }
    })
    .await?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, sync::Arc};

    use http::Method;

    use crate::{
        providers::{
            backends::youtube_api::{
                SOURCE,
                client::YoutubeClient,
                video::{VideoListResponse, get_video},
            },
            std_values::StandardRoleNames,
            types::{ChildSource, EntrySpecificData, EntryType},
        },
        test_utils::MockHttpClient,
    };

    #[test]
    fn test_parse_iso_duration_ms() {
        assert_eq!(super::parse_iso_duration_ms("PT1H2M3S"), Some(3723000));
        assert_eq!(super::parse_iso_duration_ms("PT15M"), Some(900000));
        assert_eq!(super::parse_iso_duration_ms("PT45S"), Some(45000));
        assert_eq!(super::parse_iso_duration_ms("PT2H"), Some(7200000));
        assert_eq!(super::parse_iso_duration_ms("P1DT2H"), None); // Unsupported format
    }

    fn video_api_url(id: &str) -> String {
        YoutubeClient::build_url("videos", &[("part", super::PARTS), ("id", id)])
    }

    #[tokio::test]
    async fn test_get_video() -> anyhow::Result<()> {
        let id = "ahFZQ_lDoQ4";
        let api_key = "doesnotmatter";

        let mut http_client = MockHttpClient::new();
        http_client.add_route_json::<VideoListResponse>(
            Method::GET,
            &video_api_url(id),
            include_str!("./video_whatamess_xfade.json"),
        );

        let client = YoutubeClient::new_with_client(Arc::new(http_client), api_key.to_string())?;

        let video = get_video(&client, &format!("https://www.youtube.com/watch?v={id}")).await?;

        assert_eq!(video.release_date.as_deref(), Some("2024-12-29 13:00:03"));
        assert_eq!(video.sources.len(), 1);
        assert_eq!(
            video.sources.get(SOURCE).unwrap(),
            &HashSet::from([format!("https://youtu.be/{id}")])
        );
        assert!(matches!(
            video.specific_data,
            EntrySpecificData::Track {
                duration_ms: Some(132000),
                ..
            },
        ));
        let mut children_cursor = video.children[0].cursor();
        let (child, _) = children_cursor.next().await?.expect("expected a child");
        assert_eq!(child.entry_type, EntryType::Artist);
        assert_eq!(
            child.sources.get(SOURCE).unwrap(),
            &HashSet::from(
                ["https://www.youtube.com/channel/UCqm3BQLlJfvkTsX_hvm0UmA".to_string()]
            )
        );
        assert_eq!(child.name.as_ref().unwrap(), "Watame Ch. 角巻わため");
        assert_eq!(child.contributions.len(), 1);
        let contribution = &child.contributions[0];
        assert_eq!(contribution.role, StandardRoleNames::UPLOADER);
        assert!(children_cursor.next().await?.is_none());

        assert_eq!(video.aliases.len(), 1);
        let alias = &video.aliases[0];
        assert_eq!(
            alias.name,
            "角巻わため 新EP『WHAT A MESS!!!』クロスフェード"
        );

        Ok(())
    }
}
