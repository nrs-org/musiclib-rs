use std::sync::Arc;

use serde::Deserialize;

use crate::providers::{
    FetchProvider, TryDefault,
    backends::{discogs, local, musicbrainz, nicovideo, soundcloud, spotify, youtube_api},
    types::Error,
};

fn default_true() -> bool {
    true
}

/// Per-backend enable flags. All backends are enabled by default.
/// Deserializes from YAML; omitted fields default to `true`.
///
/// Example YAML:
/// ```yaml
/// youtube_api: true
/// spotify: false
/// musicbrainz: true
/// discogs: false
/// soundcloud: true
/// nicovideo: true
/// local: true
/// ```
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct RegistryConfig {
    #[serde(default = "default_true")]
    pub youtube_api: bool,
    #[serde(default = "default_true")]
    pub spotify: bool,
    #[serde(default = "default_true")]
    pub musicbrainz: bool,
    #[serde(default = "default_true")]
    pub discogs: bool,
    #[serde(default = "default_true")]
    pub soundcloud: bool,
    #[serde(default = "default_true")]
    pub nicovideo: bool,
    #[serde(default = "default_true")]
    pub local: bool,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            youtube_api: true,
            spotify: true,
            musicbrainz: true,
            discogs: true,
            soundcloud: true,
            nicovideo: true,
            local: true,
        }
    }
}

/// Attempt to instantiate a provider via `TryDefault`, returning:
/// - `Ok(Some(arc))` on success
/// - `Ok(None)` if credentials are missing (logged as a warning)
/// - `Err` for any other failure
fn try_init<P>(name: &str) -> Result<Option<Arc<dyn FetchProvider>>, Error>
where
    P: FetchProvider + TryDefault<Error = Error> + 'static,
{
    match P::try_default() {
        Ok(p) => Ok(Some(Arc::new(p))),
        Err(Error::MissingCredentials(msg)) => {
            eprintln!("[registry] skipping {name}: missing credentials ({msg})");
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// Build the list of enabled providers from `config`.
/// Providers with missing credentials are skipped with a warning.
pub fn build_providers(config: &RegistryConfig) -> Result<Vec<Arc<dyn FetchProvider>>, Error> {
    let mut providers: Vec<Arc<dyn FetchProvider>> = Vec::new();

    macro_rules! register {
        ($flag:expr, $name:literal, $type:ty) => {
            if $flag {
                if let Some(p) = try_init::<$type>($name)? {
                    providers.push(p);
                }
            }
        };
    }

    register!(config.youtube_api, "youtube_api", youtube_api::Provider);
    register!(config.spotify, "spotify", spotify::Provider);
    register!(
        config.musicbrainz,
        "musicbrainz",
        musicbrainz::types::Provider
    );
    register!(config.discogs, "discogs", discogs::Provider);
    register!(config.soundcloud, "soundcloud", soundcloud::Provider);
    register!(config.nicovideo, "nicovideo", nicovideo::Provider);
    register!(config.local, "local", local::Provider);

    Ok(providers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_omitted_fields_default_to_true() {
        let config: RegistryConfig = serde_yaml_ng::from_str("youtube_api: false").unwrap();
        assert!(!config.youtube_api);
        assert!(config.spotify);
        assert!(config.musicbrainz);
        assert!(config.discogs);
        assert!(config.soundcloud);
        assert!(config.nicovideo);
        assert!(config.local);
    }

    #[test]
    fn test_empty_config_enables_all() {
        let config: RegistryConfig = serde_yaml_ng::from_str("{}").unwrap();
        assert!(config.youtube_api);
        assert!(config.spotify);
        assert!(config.musicbrainz);
        assert!(config.discogs);
        assert!(config.soundcloud);
        assert!(config.nicovideo);
        assert!(config.local);
    }
}
