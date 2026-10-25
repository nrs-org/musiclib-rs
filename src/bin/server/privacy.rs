//! Who can see a YouTube source (`Privacy`, stored per pair in
//! `pair_privacy`): what YouTube says, recorded on fetch or looked up here
//! when nothing is stored yet, and the user's own call from the entry page.
//! Every other source is public. Whatever shares from the library
//! (`discord_presence`) shares only a source this says is public.

use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
    time::Duration,
};

use hyper::StatusCode;
use musiclib_rs::{
    musicdb::{MusicDb, PairPrivacy},
    providers::types::Privacy,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    respond::{self, HyperResponse},
    state::AppState,
};

type Pair = (String, String);

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest client")
});

/// The Data API takes this many ids a request.
const BATCH: usize = 50;

/// A YouTube resource whose privacy the Data API reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum YouTubeKind {
    Video,
    Playlist,
}

impl YouTubeKind {
    fn endpoint(self) -> &'static str {
        match self {
            YouTubeKind::Video => "videos",
            YouTubeKind::Playlist => "playlists",
        }
    }

    /// The canonical identifier of the YouTube pair for `id`.
    pub fn identifier(self, id: &str) -> String {
        match self {
            YouTubeKind::Video => format!("https://youtu.be/{id}"),
            YouTubeKind::Playlist => format!("https://www.youtube.com/playlist?list={id}"),
        }
    }
}

/// A YouTube id, as far as it's safe to put in a query string.
pub fn youtube_id(id: &str) -> Option<&str> {
    (!id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
    .then_some(id)
}

/// The video or playlist a YouTube pair's canonical identifier names.
fn youtube_resource(identifier: &str) -> Option<(YouTubeKind, &str)> {
    if let Some(id) = identifier.strip_prefix("https://youtu.be/") {
        return Some((YouTubeKind::Video, youtube_id(id)?));
    }
    let id = identifier.strip_prefix("https://www.youtube.com/playlist?list=")?;
    Some((YouTubeKind::Playlist, youtube_id(id)?))
}

/// Looks up and stores YouTube's privacy for the YouTube videos and
/// playlists of `pairs` that have none stored, with `$YOUTUBE_API_KEY`
/// (without one, they stay unknown).
pub async fn fill_youtube(db: &MusicDb, pairs: &[Pair]) -> anyhow::Result<()> {
    let youtube: Vec<Pair> = pairs.iter().filter(|p| p.0 == "youtube").cloned().collect();
    if youtube.is_empty() {
        return Ok(());
    }
    let Some(key) = std::env::var("YOUTUBE_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
    else {
        return Ok(());
    };
    let known = db.pair_privacies(&youtube).await?;
    let mut wanted: HashMap<YouTubeKind, Vec<&str>> = HashMap::new();
    for (_, identifier) in &youtube {
        if known
            .get(&("youtube".to_owned(), identifier.clone()))
            .is_some_and(|k| k.provider.is_some())
        {
            continue;
        }
        if let Some((kind, id)) = youtube_resource(identifier) {
            wanted.entry(kind).or_default().push(id);
        }
    }
    for (kind, ids) in wanted {
        for chunk in ids.chunks(BATCH) {
            let body: Value = HTTP
                .get(format!(
                    "https://www.googleapis.com/youtube/v3/{}?part=status\
                     &id={}&key={}",
                    kind.endpoint(),
                    chunk.join(","),
                    urlencoding::encode(&key)
                ))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            for (id, privacy) in answered(chunk, &body) {
                db.set_provider_privacy("youtube", &kind.identifier(id), privacy)
                    .await?;
            }
        }
    }
    Ok(())
}

/// What a `videos`/`playlists` answer says of each of `ids`. One it leaves
/// out is private or deleted, which only its owner sees either way.
fn answered<'a>(ids: &[&'a str], body: &Value) -> Vec<(&'a str, Privacy)> {
    let listed: HashMap<&str, Option<Privacy>> = body["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let status = item["status"]["privacyStatus"].as_str();
            Some((item["id"].as_str()?, status.and_then(Privacy::parse)))
        })
        .collect();
    ids.iter()
        .filter_map(|&id| match listed.get(id) {
            Some(privacy) => privacy.map(|p| (id, p)),
            None => Some((id, Privacy::Private)),
        })
        .collect()
}

/// Whether a pair may be shared outside the library: any non-YouTube one,
/// and a YouTube one stored as public. A YouTube one with nothing stored
/// (its lookup failed) isn't.
pub fn shareable(pair: &Pair, stored: Option<&PairPrivacy>) -> bool {
    pair.0 != "youtube" || stored.and_then(PairPrivacy::effective) == Some(Privacy::Public)
}

/// `shareable` for one pair, looking YouTube's privacy up first if it
/// isn't stored. A failed lookup leaves it unknown, so not shareable.
pub async fn check_shareable(db: &MusicDb, pair: &Pair) -> bool {
    if pair.0 != "youtube" {
        return true;
    }
    let pairs = std::slice::from_ref(pair);
    if let Err(e) = fill_youtube(db, pairs).await {
        tracing::warn!("YouTube privacy lookup failed: {e:#}");
    }
    match db.pair_privacies(pairs).await {
        Ok(stored) => shareable(pair, stored.get(pair)),
        Err(e) => {
            tracing::warn!("couldn't read source privacy: {e:#}");
            false
        }
    }
}

/// The entry page's `privacy` list: each of `pairs` with a stored privacy,
/// after looking YouTube's up for those without.
pub async fn for_entry(db: &MusicDb, pairs: &[Pair]) -> Value {
    if let Err(e) = fill_youtube(db, pairs).await {
        tracing::warn!("YouTube privacy lookup failed: {e:#}");
    }
    let stored = db.pair_privacies(pairs).await.unwrap_or_else(|e| {
        tracing::warn!("couldn't read source privacy: {e:#}");
        HashMap::new()
    });
    pairs
        .iter()
        .filter_map(|pair| {
            let p = stored.get(pair)?;
            Some(json!({
                "source": pair.0,
                "identifier": pair.1,
                "provider": p.provider,
                "manual": p.manual,
            }))
        })
        .collect()
}

#[derive(Deserialize)]
struct SetRequest {
    source: String,
    identifier: String,
    /// `None` goes back to what the provider says.
    privacy: Option<Privacy>,
}

/// `POST /api/source-privacy` — `{source, identifier, privacy}`: the user's
/// own call on who can see a YouTube source (`"public"`, `"unlisted"`,
/// `"private"`), or `null` to go by what YouTube says.
pub async fn set(state: &Arc<AppState>, body: &[u8]) -> HyperResponse {
    let req: SetRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(e) => return respond::error(StatusCode::BAD_REQUEST, "invalid_input", &e.to_string()),
    };
    if req.source != "youtube" {
        return respond::error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "only YouTube sources have a privacy",
        );
    }
    match state
        .db
        .set_manual_privacy(&req.source, &req.identifier, req.privacy)
        .await
    {
        Ok(()) => respond::json(
            StatusCode::OK,
            json!({ "source": req.source, "identifier": req.identifier, "manual": req.privacy }),
        ),
        Err(e) => respond::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            &e.to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(source: &str, identifier: &str) -> Pair {
        (source.into(), identifier.into())
    }

    #[test]
    fn youtube_identifiers_name_a_video_or_playlist() {
        assert_eq!(
            youtube_resource("https://youtu.be/cZ12Tjci0LA"),
            Some((YouTubeKind::Video, "cZ12Tjci0LA"))
        );
        assert_eq!(
            youtube_resource("https://www.youtube.com/playlist?list=OLAK5uy_k0"),
            Some((YouTubeKind::Playlist, "OLAK5uy_k0"))
        );
        assert_eq!(youtube_resource("https://youtu.be/a&key=x"), None);
        assert_eq!(youtube_resource("https://www.youtube.com/@channel"), None);
    }

    #[test]
    fn an_answer_leaving_an_id_out_means_private() {
        let body = json!({ "items": [
            { "id": "a", "status": { "privacyStatus": "public" } },
            { "id": "b", "status": { "privacyStatus": "unlisted" } },
        ] });
        assert_eq!(
            answered(&["a", "b", "c"], &body),
            vec![
                ("a", Privacy::Public),
                ("b", Privacy::Unlisted),
                ("c", Privacy::Private)
            ]
        );
    }

    #[test]
    fn only_public_youtube_is_shareable() {
        let video = pair("youtube", "https://youtu.be/x");
        let stored = |provider, manual| PairPrivacy { provider, manual };
        assert!(!shareable(&video, None));
        assert!(shareable(
            &video,
            Some(&stored(Some(Privacy::Public), None))
        ));
        assert!(!shareable(
            &video,
            Some(&stored(Some(Privacy::Unlisted), None))
        ));
        // The user's call wins either way.
        assert!(shareable(
            &video,
            Some(&stored(Some(Privacy::Unlisted), Some(Privacy::Public)))
        ));
        assert!(!shareable(
            &video,
            Some(&stored(Some(Privacy::Public), Some(Privacy::Private)))
        ));
        // Everything else is public.
        assert!(shareable(
            &pair("spotify", "https://open.spotify.com/track/abc"),
            None
        ));
        assert!(shareable(
            &pair("soundcloud", "https://soundcloud.com/a/b"),
            None
        ));
        assert!(shareable(
            &pair("nicovideo", "https://www.nicovideo.jp/watch/sm1"),
            None
        ));
    }
}
