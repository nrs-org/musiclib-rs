use std::sync::Arc;

use http::HeaderValue;
use serde::Deserialize;

use crate::{
    http::HttpClient,
    providers::{
        FetchProvider,
        backends::{discogs, local, musicbrainz, nicovideo, soundcloud, spotify, youtube_api},
        types::Error,
    },
};

/// A credential value — either a literal string or a reference to an environment variable.
///
/// YAML examples:
/// ```yaml
/// api_key: "literal-value"
/// api_key: { env: "MY_ENV_VAR" }
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Credential {
    Value(String),
    Env { env: String },
}

impl Credential {
    fn from_env(var: &str) -> Self {
        Self::Env {
            env: var.to_owned(),
        }
    }

    pub fn resolve(&self) -> Option<String> {
        match self {
            Self::Value(v) => Some(v.clone()),
            Self::Env { env } => std::env::var(env).ok(),
        }
    }
}

// ── Per-backend configs ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct YoutubeApiConfig {
    #[serde(default = "YoutubeApiConfig::default_api_key")]
    pub api_key: Credential,
    #[serde(default = "YoutubeApiConfig::default_ytmusicapi_url")]
    pub ytmusicapi_server_url: Option<Credential>,
}

impl YoutubeApiConfig {
    fn default_api_key() -> Credential {
        Credential::from_env("YOUTUBE_API_KEY")
    }
    fn default_ytmusicapi_url() -> Option<Credential> {
        Some(Credential::from_env("YTMUSICAPI_SERVER_URL"))
    }
}

impl Default for YoutubeApiConfig {
    fn default() -> Self {
        Self {
            api_key: Self::default_api_key(),
            ytmusicapi_server_url: Self::default_ytmusicapi_url(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SpotifyConfig {
    #[serde(default = "SpotifyConfig::default_client_id")]
    pub client_id: Credential,
    #[serde(default = "SpotifyConfig::default_client_secret")]
    pub client_secret: Credential,
}

impl SpotifyConfig {
    fn default_client_id() -> Credential {
        Credential::from_env("SPOTIFY_CLIENT_ID")
    }
    fn default_client_secret() -> Credential {
        Credential::from_env("SPOTIFY_CLIENT_SECRET")
    }
}

impl Default for SpotifyConfig {
    fn default() -> Self {
        Self {
            client_id: Self::default_client_id(),
            client_secret: Self::default_client_secret(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct MusicBrainzConfig {
    #[serde(default = "MusicBrainzConfig::default_token")]
    pub token: Option<Credential>,
    /// Override the MusicBrainz API base URL (e.g. for a self-hosted server).
    /// Defaults to the `MUSICBRAINZ_BASE_URL` env var; falls back to the
    /// public musicbrainz.org endpoint when unset.
    #[serde(default = "MusicBrainzConfig::default_base_url")]
    pub base_url: Option<Credential>,
}

impl MusicBrainzConfig {
    fn default_token() -> Option<Credential> {
        Some(Credential::from_env("MUSICBRAINZ_TOKEN"))
    }
    fn default_base_url() -> Option<Credential> {
        Some(Credential::from_env("MUSICBRAINZ_BASE_URL"))
    }
}

impl Default for MusicBrainzConfig {
    fn default() -> Self {
        Self {
            token: Self::default_token(),
            base_url: Self::default_base_url(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct DiscogsConfig {
    #[serde(default = "DiscogsConfig::default_token")]
    pub user_token: Option<Credential>,
}

impl DiscogsConfig {
    fn default_token() -> Option<Credential> {
        Some(Credential::from_env("DISCOGS_USER_TOKEN"))
    }
}

impl Default for DiscogsConfig {
    fn default() -> Self {
        Self {
            user_token: Self::default_token(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct YtdlpConfig {
    #[serde(default = "YtdlpConfig::default_server_url")]
    pub server_url: Credential,
}

impl YtdlpConfig {
    fn default_server_url() -> Credential {
        Credential::from_env("YTDLP_SERVER_URL")
    }
}

impl Default for YtdlpConfig {
    fn default() -> Self {
        Self {
            server_url: Self::default_server_url(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct LocalConfig {
    /// Base directory for resolving relative `local://` paths. Defaults to CWD.
    pub base_dir: Option<std::path::PathBuf>,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self { base_dir: None }
    }
}

// ── Registry config ───────────────────────────────────────────────────────────

/// Top-level provider registry config.
///
/// Each backend is `Option<Config>` — set to `~` (null) to disable. Omitting a
/// backend entirely uses the default config, which reads credentials from the
/// standard environment variables (compatible with a `.env` file).
///
/// Example YAML:
/// ```yaml
/// youtube_api:
///   api_key: { env: "YOUTUBE_API_KEY" }
///   ytmusicapi_server_url: "http://localhost:8080"
///
/// spotify:
///   client_id: { env: "SPOTIFY_CLIENT_ID" }
///   client_secret: { env: "SPOTIFY_CLIENT_SECRET" }
///
/// musicbrainz:
///   token: { env: "MUSICBRAINZ_TOKEN" }
///
/// discogs: ~      # disabled
/// local: false
/// ```
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct RegistryConfig {
    pub youtube_api: Option<YoutubeApiConfig>,
    pub spotify: Option<SpotifyConfig>,
    pub musicbrainz: Option<MusicBrainzConfig>,
    pub discogs: Option<DiscogsConfig>,
    pub soundcloud: Option<YtdlpConfig>,
    pub nicovideo: Option<YtdlpConfig>,
    pub local: Option<LocalConfig>,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            youtube_api: Some(YoutubeApiConfig::default()),
            spotify: Some(SpotifyConfig::default()),
            musicbrainz: Some(MusicBrainzConfig::default()),
            discogs: Some(DiscogsConfig::default()),
            soundcloud: Some(YtdlpConfig::default()),
            nicovideo: Some(YtdlpConfig::default()),
            local: Some(LocalConfig::default()),
        }
    }
}

// ── Builder ───────────────────────────────────────────────────────────────────

fn skip_missing(name: &str, msg: &str) {
    eprintln!("[registry] skipping {name}: missing credentials ({msg})");
}

/// Build the list of enabled providers from `config`, sharing a single HTTP client
/// across all backends.
pub fn build_providers(
    config: &RegistryConfig,
    http: Arc<dyn HttpClient>,
) -> Result<Vec<Arc<dyn FetchProvider>>, Error> {
    let mut providers: Vec<Arc<dyn FetchProvider>> = Vec::new();

    if let Some(cfg) = &config.youtube_api {
        match cfg.api_key.resolve() {
            None => skip_missing("youtube_api", "api_key not set"),
            Some(key) => {
                let api_key = HeaderValue::from_str(&key)
                    .map_err(|e| Error::InvalidCredentials(format!("invalid api_key: {e}")))?;
                let mut provider =
                    youtube_api::Provider::from_client_and_key(Arc::clone(&http), api_key)?;
                if let Some(url_cred) = &cfg.ytmusicapi_server_url {
                    if let Some(url) = url_cred.resolve() {
                        provider = provider.with_ytmusicapi_url(url);
                    }
                }
                providers.push(Arc::new(provider));
            }
        }
    }

    if let Some(cfg) = &config.spotify {
        match (cfg.client_id.resolve(), cfg.client_secret.resolve()) {
            (Some(id), Some(secret)) => {
                providers.push(Arc::new(spotify::Provider::new_with_http_client(
                    Arc::clone(&http),
                    &id,
                    &secret,
                )));
            }
            _ => skip_missing("spotify", "client_id or client_secret not set"),
        }
    }

    if let Some(cfg) = &config.musicbrainz {
        let token = cfg.token.as_ref().and_then(|c| c.resolve());
        let base_url = cfg.base_url.as_ref().and_then(|c| c.resolve());
        providers.push(Arc::new(
            musicbrainz::types::Provider::new_with_client_and_base_url(
                Arc::clone(&http),
                token,
                base_url,
            )?,
        ));
    }

    if let Some(cfg) = &config.discogs {
        let token = cfg.user_token.as_ref().and_then(|c| c.resolve());
        providers.push(Arc::new(discogs::Provider::new_with_client(
            Arc::clone(&http),
            token,
        )?));
    }

    if let Some(cfg) = &config.soundcloud {
        match cfg.server_url.resolve() {
            None => skip_missing("soundcloud", "server_url not set"),
            Some(url) => {
                providers.push(Arc::new(soundcloud::Provider::new(Arc::clone(&http), url)))
            }
        }
    }

    if let Some(cfg) = &config.nicovideo {
        match cfg.server_url.resolve() {
            None => skip_missing("nicovideo", "server_url not set"),
            Some(url) => providers.push(Arc::new(nicovideo::Provider::new(Arc::clone(&http), url))),
        }
    }

    if let Some(cfg) = &config.local {
        let base_dir = cfg.base_dir.clone().map(Ok).unwrap_or_else(|| {
            std::env::current_dir().map_err(|e| Error::InvalidCredentials(e.to_string()))
        })?;
        providers.push(Arc::new(local::Provider::new(base_dir)));
    }

    Ok(providers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_null_disables_backend() {
        let config: RegistryConfig = serde_yaml_ng::from_str("youtube_api: ~").unwrap();
        assert!(config.youtube_api.is_none());
        assert!(config.spotify.is_some());
    }

    #[test]
    fn test_empty_config_enables_all() {
        let config: RegistryConfig = serde_yaml_ng::from_str("{}").unwrap();
        assert!(config.youtube_api.is_some());
        assert!(config.spotify.is_some());
        assert!(config.musicbrainz.is_some());
        assert!(config.discogs.is_some());
        assert!(config.soundcloud.is_some());
        assert!(config.nicovideo.is_some());
        assert!(config.local.is_some());
    }

    #[test]
    fn test_literal_credential() {
        let config: RegistryConfig =
            serde_yaml_ng::from_str("youtube_api:\n  api_key: my-literal-key").unwrap();
        let cfg = config.youtube_api.unwrap();
        assert_eq!(cfg.api_key.resolve(), Some("my-literal-key".into()));
    }

    #[test]
    fn test_env_credential() {
        let config: RegistryConfig =
            serde_yaml_ng::from_str("youtube_api:\n  api_key: { env: PATH }").unwrap();
        let cfg = config.youtube_api.unwrap();
        assert!(cfg.api_key.resolve().is_some()); // PATH is always set
    }
}
