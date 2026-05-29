use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::providers::{
    backends::musicbrainz::{SOURCE, canonicalize::recording_url, client::MusicBrainzClient},
    types::{Error, ExternalSources},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum IsrcLookupResponse {
    Found(IsrcResponse),
    NotFound { error: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct IsrcResponse {
    #[serde(default)]
    pub recordings: Vec<IsrcRecording>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct IsrcRecording {
    pub id: String,
}

/// Look up an ISRC in MusicBrainz and return any linked MB recording URLs.
pub async fn lookup_isrc(
    client: &MusicBrainzClient,
    isrc: &str,
) -> Result<Option<ExternalSources>, Error> {
    let endpoint = format!("isrc/{isrc}");
    client
        .get::<IsrcLookupResponse, _, _, Error, _>(&endpoint, &[], |resp| {
            let mb_urls: HashSet<String> = match resp {
                IsrcLookupResponse::NotFound { .. } => HashSet::new(),
                IsrcLookupResponse::Found(r) => r
                    .recordings
                    .iter()
                    .map(|rec| recording_url(&rec.id))
                    .collect(),
            };
            async move {
                Ok(if mb_urls.is_empty() {
                    None
                } else {
                    Some(ExternalSources::from([(SOURCE.into(), mb_urls)]))
                })
            }
        })
        .await
}
