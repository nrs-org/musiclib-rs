use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::providers::{
    backends::spotify::{
        SOURCE, canonicalize::track_url, client::SpotifyClient, track::ExternalIds, types::Paging,
    },
    types::{Error, ExternalSources},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct IsrcSearchResponse {
    pub tracks: Paging<IsrcSearchTrack>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct IsrcSearchTrack {
    pub id: String,
    #[serde(default)]
    pub external_ids: ExternalIds,
}

/// Spotify caps `limit` at 10 for search in development mode. One ISRC
/// rarely maps to more tracks than that (single + album + compilations).
const SEARCH_LIMIT: &str = "10";

/// Search Spotify for tracks carrying `isrc` and return their track URLs.
pub async fn lookup_isrc(
    client: &SpotifyClient,
    isrc: &str,
) -> Result<Option<ExternalSources>, Error> {
    let query = format!("isrc:{isrc}");
    let isrc = isrc.to_owned();
    client
        .get::<IsrcSearchResponse, _, _, Error, _>(
            "search",
            &[("q", &query), ("type", "track"), ("limit", SEARCH_LIMIT)],
            |resp| {
                // The `isrc:` filter is exact in practice, but re-check so a
                // fuzzy fallback match can never cross-link the wrong track.
                let urls: HashSet<String> = resp
                    .tracks
                    .items
                    .iter()
                    .filter(|t| {
                        t.external_ids
                            .isrc
                            .as_deref()
                            .is_some_and(|i| i.eq_ignore_ascii_case(&isrc))
                    })
                    .map(|t| track_url(&t.id))
                    .collect();
                async move {
                    Ok(if urls.is_empty() {
                        None
                    } else {
                        Some(ExternalSources::from([(SOURCE.into(), urls)]))
                    })
                }
            },
        )
        .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{http::Method, test_utils::MockHttpClient};

    fn search_api_url(isrc: &str) -> String {
        SpotifyClient::build_url(
            "search",
            &[
                ("q", &format!("isrc:{isrc}")),
                ("type", "track"),
                ("limit", SEARCH_LIMIT),
            ],
        )
    }

    #[tokio::test]
    async fn test_lookup_isrc() -> anyhow::Result<()> {
        let isrc = "JPV752307972";
        let mut http_client = MockHttpClient::new();
        http_client.add_route_json::<IsrcSearchResponse>(
            Method::GET,
            &search_api_url(isrc),
            include_str!("./search_isrc_beautiful_circle.json"),
        );

        let client = SpotifyClient::new_with_token(Arc::new(http_client), "test_token");
        let found = lookup_isrc(&client, isrc).await?.expect("expected a match");

        // The second item carries a different ISRC and must be filtered out.
        assert_eq!(
            found.get(SOURCE).unwrap(),
            &HashSet::from(["https://open.spotify.com/track/4C3klfwXqCFv61VJ4bP7oJ".to_string()])
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_lookup_isrc_no_match() -> anyhow::Result<()> {
        let isrc = "XXX000000000";
        let mut http_client = MockHttpClient::new();
        http_client.add_route_json::<IsrcSearchResponse>(
            Method::GET,
            &search_api_url(isrc),
            r#"{"tracks":{"href":"","items":[],"limit":10,"next":null,"offset":0,"previous":null,"total":0}}"#,
        );

        let client = SpotifyClient::new_with_token(Arc::new(http_client), "test_token");
        assert!(lookup_isrc(&client, isrc).await?.is_none());
        Ok(())
    }
}
