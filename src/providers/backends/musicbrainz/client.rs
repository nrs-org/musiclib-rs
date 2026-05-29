use std::{future::Future, sync::Arc};

use serde::{Serialize, de::DeserializeOwned};

use crate::{
    http::{HeaderName, HeaderValue, HttpClient, Request, default_http_client},
    providers::{backends::build_url, types::Error},
};

pub struct MusicBrainzClient {
    client: Arc<dyn HttpClient>,
    user_agent: HeaderValue,
    token: Option<HeaderValue>,
    base_url: Arc<str>,
}

impl Clone for MusicBrainzClient {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            user_agent: self.user_agent.clone(),
            token: self.token.clone(),
            base_url: self.base_url.clone(),
        }
    }
}

// Default User-Agent as required by the MusicBrainz API ToS.
const DEFAULT_USER_AGENT: &str = "musiclib-rs/0.1.0 ( https://github.com/nrs-org/musiclib-rs )";

pub const DEFAULT_BASE_URL: &str = "https://musicbrainz.org/ws/2";

/// Wraps an API response so that unexpected JSON shapes still deserialize,
/// letting us inspect the status code before deciding what to do.
#[derive(Clone, serde::Deserialize, Serialize)]
#[serde(untagged)]
enum ApiResponse<T> {
    Ok(T),
    Err(serde_json::Value),
}

impl<T> ApiResponse<T> {
    fn into_ok(self) -> Option<T> {
        match self {
            ApiResponse::Ok(v) => Some(v),
            ApiResponse::Err(_) => None,
        }
    }
}

impl MusicBrainzClient {
    pub fn new(token: Option<String>) -> Result<Self, Error> {
        Self::new_with_client(default_http_client(), token)
    }

    pub fn new_with_client(
        client: Arc<dyn HttpClient>,
        token: Option<String>,
    ) -> Result<Self, Error> {
        Self::new_with_client_and_base_url(client, token, None)
    }

    pub fn new_with_client_and_base_url(
        client: Arc<dyn HttpClient>,
        token: Option<String>,
        base_url: Option<String>,
    ) -> Result<Self, Error> {
        let user_agent = HeaderValue::from_str(DEFAULT_USER_AGENT)
            .map_err(|_| Error::InvalidCredentials("Invalid User-Agent header value".into()))?;
        let token = token
            .map(|t| {
                HeaderValue::from_str(&format!("Bearer {t}"))
                    .map_err(|_| Error::InvalidCredentials("Invalid token header value".into()))
            })
            .transpose()?;
        let base_url: Arc<str> = base_url
            .map(|u| u.trim_end_matches('/').to_string())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
            .into();
        Ok(Self {
            client,
            user_agent,
            token,
            base_url,
        })
    }

    pub fn build_url(endpoint: &str, params: &[(&str, &str)]) -> String {
        Self::build_url_with_base(DEFAULT_BASE_URL, endpoint, params)
    }

    pub fn build_url_with_base(base: &str, endpoint: &str, params: &[(&str, &str)]) -> String {
        let url = format!("{base}/{endpoint}");
        // Always include fmt=json.
        let mut all_params: Vec<(&str, &str)> = vec![("fmt", "json")];
        all_params.extend_from_slice(params);
        build_url(url, &all_params)
    }

    pub async fn get<T, F, R, E, FR>(
        &self,
        endpoint: &str,
        params: &[(&str, &str)],
        callback: F,
    ) -> Result<R, E>
    where
        T: std::any::Any + Serialize + DeserializeOwned + Send + Sync + Clone + 'static,
        F: FnOnce(&T) -> FR,
        E: From<Error> + Send + 'static,
        FR: Future<Output = Result<R, E>> + Send + 'static,
    {
        let url = Self::build_url_with_base(&self.base_url, endpoint, params);

        let mut headers = vec![(
            HeaderName::from_static("user-agent"),
            self.user_agent.clone(),
        )];
        if let Some(token) = &self.token {
            headers.push((HeaderName::from_static("authorization"), token.clone()));
        }

        let response = self
            .client
            .get_json::<ApiResponse<T>>(Request {
                url: url.clone(),
                headers,
                ..Default::default()
            })
            .await
            .map_err(Error::from)
            .map_err(E::from)?;

        match response.status.as_u16() {
            200..=299 => {
                let data = response
                    .json::<ApiResponse<T>>()
                    .await
                    .map_err(Error::from)?
                    .clone()
                    .into_ok()
                    .ok_or_else(|| {
                        Error::InvalidUrl(format!(
                            "unexpected response shape from MusicBrainz for {url}"
                        ))
                    })
                    .map_err(E::from)?;
                callback(&data).await
            }
            404 => Err(E::from(Error::NotFound(url))),
            _ => Err(E::from(Error::Http(crate::http::Error::HttpStatus(
                response.status,
            )))),
        }
    }
}

#[cfg(test)]
mod live_tests {
    //! Live smoke tests against a self-hosted MusicBrainz server.
    //! Run with: MUSICBRAINZ_BASE_URL=http://localhost:5000/ws/2 \
    //!     cargo test -p musiclib-rs mb_live -- --ignored --nocapture
    use crate::http::default_http_client;
    use crate::providers::backends::musicbrainz::{
        artist::get_artist, canonicalize::canonicalize, client::MusicBrainzClient,
        isrc::lookup_isrc, url::lookup_url,
    };
    use crate::providers::std_values::StandardProviderKeys;

    const WATAME_MBID: &str = "201500bb-d0b7-49bf-9869-50e6496350b8";

    fn client_from_env() -> anyhow::Result<MusicBrainzClient> {
        let base = std::env::var("MUSICBRAINZ_BASE_URL")
            .map_err(|_| anyhow::anyhow!("MUSICBRAINZ_BASE_URL not set"))?;
        Ok(MusicBrainzClient::new_with_client_and_base_url(
            default_http_client(),
            None,
            Some(base),
        )?)
    }

    #[tokio::test]
    #[ignore = "requires a reachable MusicBrainz server at MUSICBRAINZ_BASE_URL"]
    async fn mb_live_canonicalize() {
        let url = format!("https://musicbrainz.org/artist/{WATAME_MBID}");
        let result =
            canonicalize(StandardProviderKeys::UNKNOWN_URL, &url).expect("canonicalize artist URL");
        println!(
            "[canonicalize] source={} identifier={} type={:?} external_type={}",
            result.canonical_source_key,
            result.canonical_identifier,
            result.entry_type,
            result.external_type,
        );
        assert_eq!(result.canonical_identifier, url);
    }

    #[tokio::test]
    #[ignore = "requires a reachable MusicBrainz server at MUSICBRAINZ_BASE_URL"]
    async fn mb_live_get_artist() -> anyhow::Result<()> {
        let client = client_from_env()?;
        let url = format!("https://musicbrainz.org/artist/{WATAME_MBID}");
        let artist = get_artist(&client, &url).await?;
        println!(
            "[get_artist] aliases={} sources={} children={}",
            artist.aliases.len(),
            artist.sources.0.len(),
            artist.children.len(),
        );
        assert!(!artist.aliases.is_empty(), "expected at least one alias");
        assert_eq!(artist.children.len(), 4);
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires a reachable MusicBrainz server at MUSICBRAINZ_BASE_URL"]
    async fn mb_live_lookup_url() -> anyhow::Result<()> {
        let client = client_from_env()?;
        let resource = "https://www.youtube.com/channel/UCqm3BQLlJfvkTsX_hvm0UmA";
        let found = lookup_url(&client, resource).await?;
        println!("[lookup_url] {resource} -> {found:?}");
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires a reachable MusicBrainz server at MUSICBRAINZ_BASE_URL"]
    async fn mb_live_lookup_isrc() -> anyhow::Result<()> {
        let client = client_from_env()?;
        // ISRC for "Beautiful Circle" — already used in the fixture suite.
        let isrc = "JPB602202407";
        let found = lookup_isrc(&client, isrc).await?;
        println!("[lookup_isrc] {isrc} -> {found:?}");
        Ok(())
    }
}
