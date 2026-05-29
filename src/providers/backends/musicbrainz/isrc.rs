use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::providers::{
    backends::musicbrainz::{SOURCE, canonicalize::recording_url, client::MusicBrainzClient},
    types::{Error, ExternalSources},
};

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
    let result = client
        .get::<IsrcResponse, _, _, Error, _>(&endpoint, &[], |resp| {
            let mb_urls: HashSet<String> = resp
                .recordings
                .iter()
                .map(|rec| recording_url(&rec.id))
                .collect();
            async move {
                Ok(if mb_urls.is_empty() {
                    None
                } else {
                    Some(ExternalSources::from([(SOURCE.into(), mb_urls)]))
                })
            }
        })
        .await;
    match result {
        Ok(found) => Ok(found),
        Err(Error::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}
