use std::{collections::HashSet, future::Future, sync::Arc};

use serde::{Deserialize, de::DeserializeOwned};

use crate::providers::{
    backends::{nicovideo::SOURCE, ytdlp::YtdlpClient},
    types::{
        Alias, CachedChildSource, ChildRef, CompiledChildMatcher, CompiledEntryDataMatcher,
        CompiledMatcherExpr, EntityResult, EntrySpecificData, EntryType, Error, ExternalSources,
        Tribool, default_eval_leaf, static_eval_expr,
    },
};

use super::EXTERNAL_TYPE_VIDEO;

/// What every item of a user's uploads listing is, known without fetching it:
/// the endpoint only returns the user's own videos. Lets an artist's fetch
/// options skip the listing entirely when they'd drop every video (see
/// `matcher::filter_children`), so the full-upload yt-dlp call (tens of
/// thousands of videos for some accounts) never runs. Only state facts the
/// endpoint guarantees; anything else must stay `Indeterminate`.
fn upload_eval(expr: &CompiledMatcherExpr) -> Tribool {
    static_eval_expr(expr, &|matcher| match matcher {
        CompiledChildMatcher::EntryData(CompiledEntryDataMatcher::EntryType(t)) => {
            (*t == EntryType::Track).into()
        }
        CompiledChildMatcher::EntryData(CompiledEntryDataMatcher::ExternalType(t)) => {
            (t == EXTERNAL_TYPE_VIDEO).into()
        }
        CompiledChildMatcher::EntryData(CompiledEntryDataMatcher::AppearsOn(want)) => {
            (!*want).into()
        }
        _ => default_eval_leaf(matcher),
    })
}

#[derive(Clone, Deserialize)]
pub struct UserResponse {
    pub webpage_url: String,
    pub title: Option<String>,
    #[serde(default)]
    pub entries: Vec<FlatVideoEntry>,
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

#[derive(Clone, Deserialize)]
pub struct FlatVideoEntry {
    pub url: String,
}

pub async fn get_user_raw<T, F, E, FR, R>(
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

/// Fetches user metadata without entries (fast), then returns an `EntityResult`
/// whose video children are loaded lazily on first iteration.
pub async fn get_user(client: Arc<YtdlpClient>, url: &str) -> Result<EntityResult<()>, Error> {
    let meta: UserResponse = serde_json::from_value(client.fetch_no_children(url).await?)
        .map_err(|e| Error::InvalidUrl(format!("failed to deserialize user metadata: {e}")))?;

    let mut sources = ExternalSources::default();
    sources
        .0
        .entry(SOURCE.into())
        .or_default()
        .insert(meta.webpage_url);

    let video_source = client
        .lazy_children(url.to_string(), |value| {
            let response: UserResponse = serde_json::from_value(value).map_err(|e| {
                Error::InvalidUrl(format!("failed to deserialize user videos: {e}"))
            })?;
            Ok(response
                .entries
                .into_iter()
                .map(|entry| ChildRef {
                    entry_type: EntryType::Track,
                    external_type: EXTERNAL_TYPE_VIDEO.into(),
                    sources: [(SOURCE.into(), HashSet::from([entry.url]))].into(),
                    ..Default::default()
                })
                .collect())
        })
        .with_static_eval(upload_eval);

    Ok(EntityResult {
        release_date: None,
        sources,
        extra: meta.extra,
        specific_data: EntrySpecificData::Artist,
        children: vec![Arc::new(CachedChildSource::new(Box::new(video_source)))],
        aliases: meta
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
    })
}

#[cfg(test)]
mod tests {
    use crate::providers::types::child_next;
    use std::sync::Arc;

    use crate::{
        http::Method,
        providers::{
            backends::{nicovideo::SOURCE, ytdlp::YtdlpClient},
            types::{EntrySpecificData, EntryType},
        },
        test_utils::MockHttpClient,
    };

    use super::get_user;

    const SERVER: &str = "http://mock";

    fn entity_url(url: &str) -> String {
        format!("{}/entities/{}", SERVER, urlencoding::encode(url))
    }

    fn make_client(url: &str, fixture: &'static str) -> Arc<YtdlpClient> {
        let mut http = MockHttpClient::new();
        http.add_route_json::<serde_json::Value>(
            Method::GET,
            &format!("{}?no_entries", entity_url(url)),
            fixture,
        );
        http.add_route_json::<serde_json::Value>(Method::GET, &entity_url(url), fixture);
        Arc::new(YtdlpClient::new(Arc::new(http), SERVER.to_string()))
    }

    #[tokio::test]
    async fn test_get_user() -> anyhow::Result<()> {
        let url = "https://www.nicovideo.jp/user/67047227";
        let client = make_client(url, include_str!("./user_67047227.json"));
        let user = get_user(client, url).await?;

        assert!(matches!(user.specific_data, EntrySpecificData::Artist));

        assert_eq!(
            user.sources.get(SOURCE).unwrap(),
            &std::collections::HashSet::from([url.to_string()])
        );

        assert_eq!(user.children.len(), 1);

        // The uploads listing declares its items before being read, so an
        // artist's filter can skip it without the full yt-dlp call.
        {
            use crate::providers::types::{
                ChildSource, CompiledChildMatcher, CompiledEntryDataMatcher, CompiledMatcherExpr,
                Tribool,
            };
            let is = |t: &str| {
                CompiledMatcherExpr::Matcher(CompiledChildMatcher::EntryData(
                    CompiledEntryDataMatcher::ExternalType(t.to_string()),
                ))
            };
            let unread = user.children[0].cursor();
            assert!(unread.evaluate_expr(&is("nicovideo:video")) == Tribool::True);
            assert!(unread.evaluate_expr(&is("youtube:video")) == Tribool::False);
        }

        let mut videos = user.children[0].cursor();
        let (first_video, _) = child_next(&mut videos)
            .await?
            .expect("expected first video");
        assert_eq!(first_video.entry_type, EntryType::Track);

        Ok(())
    }
}
