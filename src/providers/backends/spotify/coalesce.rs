//! Coalesce rules for Spotify's "several X" batch endpoints.
//!
//! Spotify exposes a single-entity getter (`GET /v1/tracks/{id}`) *and* a batch
//! getter (`GET /v1/tracks?ids=A,B,C`) for tracks, albums and artists. The batch
//! form costs one rate-limit unit regardless of id count, so coalescing the
//! importer's per-entity fan-out is a straight win.
//!
//! Unlike YouTube — where the single and batch forms share a path and response
//! shape — Spotify differs in **both**:
//!
//! | resource | single                  | batch                       | batch limit |
//! |----------|-------------------------|-----------------------------|-------------|
//! | tracks   | `GET /v1/tracks/{id}`   | `GET /v1/tracks?ids=…`      | 50          |
//! | albums   | `GET /v1/albums/{id}`   | `GET /v1/albums?ids=…`      | 20          |
//! | artists  | `GET /v1/artists/{id}`  | `GET /v1/artists?ids=…`     | 50          |
//!
//! - The single form returns a **bare** resource object (`{ "id": …, … }`).
//! - The batch form wraps them: `{ "tracks": [<track|null>, …] }`, positionally
//!   aligned to the requested ids with `null` for ones Spotify couldn't return.
//!
//! So [`merge`](SpotifyBatchRule::merge) rewrites the path-segment id form into
//! the `?ids=` query form, and [`split`](SpotifyBatchRule::split) *unwraps* each
//! element back to a bare object — byte-identical to what the single-entity
//! endpoint (and therefore the per-id cache key) expects. Items are matched by
//! their own `id` field rather than array position, so `null`s and any
//! reordering are handled robustly.
//!
//! `GET /v1/playlists/{id}` has no batch equivalent and is intentionally not
//! covered.

use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    sync::Arc,
};

use bytes::Bytes;

use crate::http::{
    BodyExtractError, CoalesceKey, CoalesceRule, Error, HeaderName, HeaderValue, Method, Request,
    ResponseStatus, SplitResponse,
};

const SPOTIFY_HOST: &str = "api.spotify.com";

/// Generic coalesce rule for a Spotify batch resource (`tracks`/`albums`/
/// `artists`). The batch response wraps results in a key equal to `resource`.
pub struct SpotifyBatchRule {
    /// Stable identifier for diagnostics, e.g. `"spotify_tracks"`.
    name: &'static str,
    /// Resource path/JSON segment, e.g. `"tracks"`.
    resource: &'static str,
    /// Batch ceiling: 50 for tracks/artists, 20 for albums.
    max_batch: usize,
}

impl SpotifyBatchRule {
    /// `GET /v1/tracks?ids=…` — up to 50 ids.
    pub fn tracks() -> Arc<Self> {
        Arc::new(Self {
            name: "spotify_tracks",
            resource: "tracks",
            max_batch: 50,
        })
    }

    /// `GET /v1/albums?ids=…` — up to 20 ids.
    pub fn albums() -> Arc<Self> {
        Arc::new(Self {
            name: "spotify_albums",
            resource: "albums",
            max_batch: 20,
        })
    }

    /// `GET /v1/artists?ids=…` — up to 50 ids.
    pub fn artists() -> Arc<Self> {
        Arc::new(Self {
            name: "spotify_artists",
            resource: "artists",
            max_batch: 50,
        })
    }

    /// Returns the entity id iff `req` is a single-entity GET for this resource,
    /// i.e. the path is exactly `/v1/{resource}/{id}`. Deeper paths such as
    /// `/v1/albums/{id}/tracks` and the batch path `/v1/{resource}` are rejected.
    fn id_for(resource: &str, req: &Request) -> Option<String> {
        let url = reqwest::Url::parse(&req.url).ok()?;
        if url.host_str() != Some(SPOTIFY_HOST) {
            return None;
        }
        let segs: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
        match segs.as_slice() {
            ["v1", r, id] if *r == resource => Some((*id).to_string()),
            _ => None,
        }
    }
}

impl CoalesceRule for SpotifyBatchRule {
    fn name(&self) -> &'static str {
        self.name
    }

    fn group(&self, req: &Request) -> Option<CoalesceKey> {
        if req.method != Method::GET {
            return None;
        }
        // Only single-entity requests for this resource are batchable.
        Self::id_for(self.resource, req)?;
        let url = reqwest::Url::parse(&req.url).ok()?;

        let mut hasher = DefaultHasher::new();
        SPOTIFY_HOST.hash(&mut hasher);
        self.resource.hash(&mut hasher);

        // Headers — include so requests with different bearer tokens never merge.
        let mut headers: Vec<&(HeaderName, HeaderValue)> = req.headers.iter().collect();
        headers.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        for (k, v) in &headers {
            k.as_str().hash(&mut hasher);
            v.as_bytes().hash(&mut hasher);
        }

        // Any query params (other than the batch `ids`) influence the response
        // — e.g. `market`. Single-entity calls in this codebase carry none, but
        // hashing them keeps the rule correct if that ever changes.
        let mut params: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(k, _)| k != "ids")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        params.sort();
        for (k, v) in &params {
            k.hash(&mut hasher);
            v.hash(&mut hasher);
        }

        Some(CoalesceKey(hasher.finish()))
    }

    fn max_batch(&self) -> usize {
        self.max_batch
    }

    fn merge(&self, reqs: &[Request]) -> Request {
        let template = &reqs[0];
        let mut url = reqwest::Url::parse(&template.url)
            .expect("group() only accepts parseable URLs; merge() can't see one that isn't");

        let ids: Vec<String> = reqs
            .iter()
            .filter_map(|r| Self::id_for(self.resource, r))
            .collect();
        let preserved: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(k, _)| k != "ids")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

        // Path-segment id form (`/v1/tracks/{id}`) → batch form (`/v1/tracks`).
        url.set_path(&format!("/v1/{}", self.resource));
        url.query_pairs_mut().clear();
        for (k, v) in &preserved {
            url.query_pairs_mut().append_pair(k, v);
        }
        url.query_pairs_mut().append_pair("ids", &ids.join(","));

        Request {
            method: template.method.clone(),
            url: url.into(),
            headers: template.headers.clone(),
            body: template.body.clone(),
            ..Default::default()
        }
    }

    fn split(
        &self,
        merged_status: ResponseStatus,
        merged_headers: &[(HeaderName, HeaderValue)],
        merged_body: &Bytes,
        reqs: &[Request],
    ) -> Result<Vec<Option<SplitResponse>>, Arc<Error>> {
        // Failures: hand every waiter the same status + body so each caller's
        // own status handling (e.g. the 401 token-refresh retry) still fires.
        if !merged_status.is_success() {
            let body = merged_body.clone();
            return Ok((0..reqs.len())
                .map(|_| {
                    Some(SplitResponse {
                        status: merged_status,
                        headers: merged_headers.to_vec(),
                        body: body.clone(),
                    })
                })
                .collect());
        }

        #[derive(serde::Deserialize)]
        struct ItemId {
            id: String,
        }

        // Parse outer wrapper as raw values to avoid parsing item content.
        let root: HashMap<String, Box<serde_json::value::RawValue>> =
            serde_json::from_slice(merged_body)
                .map_err(|e| Arc::new(Error::from(BodyExtractError::Json(e))))?;

        // `{ "<resource>": [<obj|null>, …] }`. Build an id → bare-object lookup,
        // skipping `null`s (Spotify's marker for ids it couldn't resolve).
        let items: Vec<Option<Box<serde_json::value::RawValue>>> = root
            .get(self.resource)
            .map(|raw: &Box<serde_json::value::RawValue>| {
                serde_json::from_str::<Vec<Option<Box<serde_json::value::RawValue>>>>(raw.get())
            })
            .transpose()
            .map_err(|e| Arc::new(Error::from(BodyExtractError::Json(e))))?
            .unwrap_or_default();

        let by_id: HashMap<String, Box<serde_json::value::RawValue>> = items
            .into_iter()
            .flatten()
            .filter_map(|item: Box<serde_json::value::RawValue>| {
                let id: ItemId = serde_json::from_str(item.get()).ok()?;
                Some((id.id, item))
            })
            .collect();

        let mut out = Vec::with_capacity(reqs.len());
        for req in reqs {
            let Some(id) = Self::id_for(self.resource, req) else {
                out.push(None);
                continue;
            };
            match by_id.get(&id) {
                Some(item) => {
                    // Emit the bare object — byte-identical to a single-entity
                    // GET, which is what the per-id cache key stores.
                    let body = serde_json::to_vec(item)
                        .map_err(|e| Arc::new(Error::from(BodyExtractError::Json(e))))?;
                    out.push(Some(SplitResponse {
                        status: merged_status,
                        headers: merged_headers.to_vec(),
                        body: Bytes::from(body),
                    }));
                }
                None => out.push(None),
            }
        }

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::{SpotifyBatchRule, *};
    use crate::http::{Method, Request, ResponseStatus};

    fn track_req(id: &str) -> Request {
        Request {
            method: Method::GET,
            url: format!("https://api.spotify.com/v1/tracks/{id}"),
            ..Default::default()
        }
    }

    #[test]
    fn group_accepts_single_entity_track() {
        let rule = SpotifyBatchRule::tracks();
        assert!(rule.group(&track_req("4C3klfwXqCFv61VJ4bP7oJ")).is_some());
    }

    #[test]
    fn group_rejects_wrong_resource() {
        // The tracks rule must not own album or artist requests.
        let rule = SpotifyBatchRule::tracks();
        for url in [
            "https://api.spotify.com/v1/albums/0LCmjkgN0CqcPtLuNHUBma",
            "https://api.spotify.com/v1/artists/68609MOnEU86kVyMf26JnM",
        ] {
            let r = Request {
                method: Method::GET,
                url: url.into(),
                ..Default::default()
            };
            assert!(rule.group(&r).is_none(), "should not own: {url}");
        }
    }

    #[test]
    fn group_rejects_subresource_and_batch_paths() {
        let albums = SpotifyBatchRule::albums();
        // album tracks sub-resource — deeper path, not a single-entity get.
        let sub = Request {
            method: Method::GET,
            url: "https://api.spotify.com/v1/albums/ABC/tracks?limit=50&offset=0".into(),
            ..Default::default()
        };
        assert!(albums.group(&sub).is_none());

        // The batch path itself must not be re-batched.
        let batch = Request {
            method: Method::GET,
            url: "https://api.spotify.com/v1/albums?ids=A,B".into(),
            ..Default::default()
        };
        assert!(albums.group(&batch).is_none());
    }

    #[test]
    fn group_rejects_other_host() {
        let rule = SpotifyBatchRule::tracks();
        let r = Request {
            method: Method::GET,
            url: "https://example.com/v1/tracks/X".into(),
            ..Default::default()
        };
        assert!(rule.group(&r).is_none());
    }

    #[test]
    fn group_keys_equal_across_ids_but_differ_across_resources() {
        let tracks = SpotifyBatchRule::tracks();
        let a = tracks.group(&track_req("AAA")).unwrap();
        let b = tracks.group(&track_req("BBB")).unwrap();
        assert_eq!(a, b, "same resource, different id → same group");

        let albums = SpotifyBatchRule::albums();
        let alb = albums
            .group(&Request {
                method: Method::GET,
                url: "https://api.spotify.com/v1/albums/AAA".into(),
                ..Default::default()
            })
            .unwrap();
        assert_ne!(a, alb, "different resource → different group");
    }

    #[test]
    fn group_keys_differ_for_different_tokens() {
        let rule = SpotifyBatchRule::tracks();
        let with_token = |tok: &str| {
            let mut r = track_req("X");
            r.headers.push((
                HeaderName::from_static("authorization"),
                HeaderValue::from_str(tok).unwrap(),
            ));
            rule.group(&r).unwrap()
        };
        assert_ne!(with_token("Bearer a"), with_token("Bearer b"));
    }

    #[test]
    fn merge_rewrites_to_ids_query_form() {
        let rule = SpotifyBatchRule::tracks();
        let reqs = vec![track_req("A"), track_req("B"), track_req("C")];
        let merged = rule.merge(&reqs);
        let url = reqwest::Url::parse(&merged.url).unwrap();
        assert_eq!(url.host_str(), Some("api.spotify.com"));
        assert_eq!(url.path(), "/v1/tracks");
        let ids = url
            .query_pairs()
            .find(|(k, _)| k == "ids")
            .map(|(_, v)| v.into_owned())
            .unwrap();
        assert_eq!(ids, "A,B,C");
    }

    #[test]
    fn split_unwraps_to_bare_objects_and_marks_missing() {
        let rule = SpotifyBatchRule::tracks();
        let body = serde_json::json!({
            "tracks": [
                { "id": "A", "name": "alpha" },
                null, // B was unresolvable
                { "id": "C", "name": "gamma" },
            ]
        });
        let merged_body = Bytes::from(serde_json::to_vec(&body).unwrap());
        let reqs = vec![track_req("A"), track_req("B"), track_req("C")];

        let splits = rule
            .split(ResponseStatus::OK, &[], &merged_body, &reqs)
            .unwrap();
        assert_eq!(splits.len(), 3);

        // A → bare object, NOT wrapped in `{ "tracks": [...] }`.
        let a = splits[0].as_ref().unwrap();
        let a_json: serde_json::Value = serde_json::from_slice(&a.body).unwrap();
        assert_eq!(a_json["id"], "A");
        assert_eq!(a_json["name"], "alpha");
        assert!(a_json.get("tracks").is_none());

        // B was null in the batch → missing → synthetic 404 downstream.
        assert!(splits[1].is_none());

        let c = splits[2].as_ref().unwrap();
        let c_json: serde_json::Value = serde_json::from_slice(&c.body).unwrap();
        assert_eq!(c_json["id"], "C");
    }

    #[test]
    fn split_matches_by_id_not_position() {
        // Batch returns items in a different order than requested.
        let rule = SpotifyBatchRule::albums();
        let body = serde_json::json!({
            "albums": [
                { "id": "C", "name": "gamma" },
                { "id": "A", "name": "alpha" },
            ]
        });
        let merged_body = Bytes::from(serde_json::to_vec(&body).unwrap());
        let reqs = vec![
            Request {
                method: Method::GET,
                url: "https://api.spotify.com/v1/albums/A".into(),
                ..Default::default()
            },
            Request {
                method: Method::GET,
                url: "https://api.spotify.com/v1/albums/C".into(),
                ..Default::default()
            },
        ];
        let splits = rule
            .split(ResponseStatus::OK, &[], &merged_body, &reqs)
            .unwrap();
        let first: serde_json::Value =
            serde_json::from_slice(&splits[0].as_ref().unwrap().body).unwrap();
        let second: serde_json::Value =
            serde_json::from_slice(&splits[1].as_ref().unwrap().body).unwrap();
        assert_eq!(first["id"], "A");
        assert_eq!(second["id"], "C");
    }

    #[test]
    fn split_on_failure_propagates_status_to_all_waiters() {
        let rule = SpotifyBatchRule::tracks();
        let merged_body = Bytes::from_static(b"{\"error\":{\"status\":401}}");
        let reqs = vec![track_req("A"), track_req("B")];
        let splits = rule
            .split(ResponseStatus::UNAUTHORIZED, &[], &merged_body, &reqs)
            .unwrap();
        for s in splits {
            let s = s.unwrap();
            assert_eq!(s.status, ResponseStatus::UNAUTHORIZED);
            assert!(!s.body.is_empty());
        }
    }
}
