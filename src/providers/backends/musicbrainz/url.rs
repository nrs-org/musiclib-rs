use std::collections::HashSet;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::providers::std_values::StandardProviderKeys;
use crate::providers::{
    backends::musicbrainz::{
        SOURCE,
        canonicalize::{artist_url, recording_url, release_group_url, release_url},
        client::MusicBrainzClient,
    },
    types::{Error, ExternalSources},
};

// --- Shared types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UrlResource {
    pub resource: String,
}

// --- Helpers ---

/// Map a URL to a source key string. Returns `StandardProviderKeys::UNKNOWN_URL` for unrecognised domains.
pub(crate) fn url_source_key(url: &str) -> &'static str {
    let Some(host) = url
        .split("//")
        .nth(1)
        .and_then(|s| s.split('/').next())
        .map(|h| h.trim_start_matches("www."))
    else {
        return StandardProviderKeys::UNKNOWN_URL;
    };
    match host {
        "youtube.com" | "youtu.be" | "music.youtube.com" => "youtube",
        "open.spotify.com" => "spotify",
        "discogs.com" => "discogs",
        "soundcloud.com" => "soundcloud",
        "nicovideo.jp" => "nicovideo",
        "music.apple.com" | "itunes.apple.com" => "apple_music",
        "tidal.com" => "tidal",
        "deezer.com" => "deezer",
        "vgmdb.net" => "vgmdb",
        "last.fm" | "lastfm.com" => "lastfm",
        _ => StandardProviderKeys::UNKNOWN_URL,
    }
}

// --- API response types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UrlResponse {
    pub id: String,
    pub resource: String,
    #[serde(default)]
    pub relations: Vec<UrlRelation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UrlRelation {
    #[serde(rename = "target-type")]
    pub target_type: String,
    pub artist: Option<UrlRelationEntity>,
    pub recording: Option<UrlRelationEntity>,
    pub release: Option<UrlRelationEntity>,
    #[serde(rename = "release-group")]
    pub release_group: Option<UrlRelationEntity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UrlRelationEntity {
    pub id: String,
}

// --- Constants ---

const PARTS: &str = "artist-rels+recording-rels+release-rels+release-group-rels";

// --- Public API ---

/// Look up a single external URL in MusicBrainz and return any linked MB entity URLs.
///
/// If `mirror_db` is provided the function first checks the local `mb_url`
/// table (keyed by `url_norm`) for the MBIDs (`gid`) of every raw MB URL that
/// normalizes to `resource_url`. A miss means MusicBrainz has no record of
/// this URL, so the API call is skipped entirely.
///
/// A hit is then looked up **by MBID** (`GET /ws/2/url/<gid>`) rather than by
/// resource string. This matters because MusicBrainz's `resource=` lookup is
/// an exact string match against whatever an editor originally pasted (often
/// with a trailing `/about`, `/videos`, a stray query string, etc.), while
/// our own canonicalizer normalizes those away — so replaying the normalized
/// string back at `resource=` frequently 404s even though the URL is known.
/// Looking up by MBID sidesteps that mismatch entirely.
///
/// Without a mirror, this falls back to the old exact-`resource=` lookup
/// against the live API. It also falls back for a mirror row that predates
/// `gid` tracking (reads back `NULL`) — presence is still confirmed, just
/// not which MBID it is, so the normalized string is the best we have until
/// the next full re-extract or backfill.
pub async fn lookup_url(
    client: &MusicBrainzClient,
    resource_url: &str,
    mirror_db: Option<&Mutex<rusqlite::Connection>>,
) -> Result<Option<ExternalSources>, Error> {
    if let Some(db) = mirror_db {
        let norm = crate::providers::registry::normalize(resource_url);
        let gids = match mirror_rows_for(db, &norm) {
            None => return Ok(None),
            Some(gids) => gids,
        };

        if gids.is_empty() {
            // Rows exist for this URL, but all predate gid tracking.
            return fetch_url_relations(client, "url", &[("resource", &norm)]).await;
        }

        let mut result = ExternalSources::default();
        for gid in gids {
            if let Some(found) = fetch_url_relations(client, &format!("url/{gid}"), &[]).await? {
                for (k, v) in found.0 {
                    result.0.entry(k).or_default().extend(v);
                }
            }
        }
        return Ok(if result.0.is_empty() {
            None
        } else {
            Some(result)
        });
    }

    fetch_url_relations(client, "url", &[("resource", resource_url)]).await
}

/// `None` if the mirror has no row at all for `norm` (MusicBrainz confidently
/// has nothing). `Some(vec)` otherwise, with one entry per matching row that
/// has a known `gid` — possibly empty if every matching row predates `gid`
/// tracking. Non-unique: several distinct raw MB URLs (protocol/case/query
/// variants) can normalize to the same form, so all of them are tried.
fn mirror_rows_for(db: &Mutex<rusqlite::Connection>, norm: &str) -> Option<Vec<Uuid>> {
    let conn = db.lock().ok()?;
    let mut stmt = conn
        .prepare("SELECT gid FROM mb_url WHERE url_norm = ?1")
        .ok()?;
    let rows = stmt
        .query_map(rusqlite::params![norm], |row| row.get::<_, Option<Uuid>>(0))
        .ok()?;
    let rows: Vec<Option<Uuid>> = rows.filter_map(Result::ok).collect();
    if rows.is_empty() {
        return None;
    }
    Some(rows.into_iter().flatten().collect())
}

/// Shared GET + relation-parsing for both the by-resource and by-MBID lookups.
/// `extra_params` supplies the endpoint-specific identifying param
/// (`resource=...`); `inc=<PARTS>` is always appended.
async fn fetch_url_relations(
    client: &MusicBrainzClient,
    endpoint: &str,
    extra_params: &[(&str, &str)],
) -> Result<Option<ExternalSources>, Error> {
    let mut params = extra_params.to_vec();
    params.push(("inc", PARTS));

    let result = client
        .get::<UrlResponse, _, _, Error, _>(endpoint, &params, |resp| {
            let mb_urls: HashSet<String> = resp
                .relations
                .iter()
                .filter_map(|rel| match rel.target_type.as_str() {
                    "artist" => rel.artist.as_ref().map(|e| artist_url(&e.id)),
                    "recording" => rel.recording.as_ref().map(|e| recording_url(&e.id)),
                    "release" => rel.release.as_ref().map(|e| release_url(&e.id)),
                    "release-group" => rel.release_group.as_ref().map(|e| release_group_url(&e.id)),
                    _ => None,
                })
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
