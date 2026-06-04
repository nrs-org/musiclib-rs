//! Coalesce rules for YouTube Data API `*.list` endpoints.
//!
//! YouTube's `videos.list`, `channels.list` and `playlists.list` all accept a
//! comma-separated `id=A,B,C` selector (up to 50 ids) and cost **one quota
//! unit** regardless of how many ids are passed. The importer fans out one
//! single-id request per entity; these rules merge the concurrent ones into a
//! single upstream call and split the response back so the existing per-entity
//! JSON deserialisers keep working unchanged.
//!
//! All three endpoints share the same response shape — a top-level `items`
//! array whose elements carry a string `id` — so one generic rule
//! ([`YouTubeListRule`]) covers them, parameterised by endpoint path.
//!
//! The `channels` and `playlists` paths are *also* used with non-id selectors
//! (`channels?forHandle=…`, `playlists?channelId=…`, `playlistItems?…`). Those
//! requests can't be batched, so [`group`](YouTubeListRule::group) only matches
//! requests that actually carry an `id` query param and lets the rest fall
//! through untouched.

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

const API_HOST: &str = "www.googleapis.com";

/// Per-call ceiling: every `*.list` endpoint allows up to 50 ids.
const MAX_IDS_PER_CALL: usize = 50;

/// Generic coalesce rule for an id-based YouTube `*.list` endpoint.
pub struct YouTubeListRule {
    /// Stable identifier for diagnostics, e.g. `"youtube_videos.list"`.
    name: &'static str,
    /// Exact request path this rule owns, e.g. `"/youtube/v3/videos"`.
    path: &'static str,
}

impl YouTubeListRule {
    /// `videos.list` — fetch video metadata by id.
    pub fn videos() -> Arc<Self> {
        Arc::new(Self {
            name: "youtube_videos.list",
            path: "/youtube/v3/videos",
        })
    }

    /// `channels.list` — fetch channel metadata by id. (The `forHandle` /
    /// `forUsername` variants carry no `id` and pass through unbatched.)
    pub fn channels() -> Arc<Self> {
        Arc::new(Self {
            name: "youtube_channels.list",
            path: "/youtube/v3/channels",
        })
    }

    /// `playlists.list` — fetch playlist metadata by id. (The `channelId`
    /// variant carries no `id` and passes through unbatched.)
    pub fn playlists() -> Arc<Self> {
        Arc::new(Self {
            name: "youtube_playlists.list",
            path: "/youtube/v3/playlists",
        })
    }

    fn extract_id(req: &Request) -> Option<String> {
        reqwest::Url::parse(&req.url)
            .ok()?
            .query_pairs()
            .find(|(k, _)| k == "id")
            .map(|(_, v)| v.into_owned())
    }
}

impl CoalesceRule for YouTubeListRule {
    fn name(&self) -> &'static str {
        self.name
    }

    fn group(&self, req: &Request) -> Option<CoalesceKey> {
        if req.method != Method::GET {
            return None;
        }
        let url = reqwest::Url::parse(&req.url).ok()?;
        if url.host_str() != Some(API_HOST) || url.path() != self.path {
            return None;
        }
        // Only id-based requests are batchable. `channels`/`playlists` are
        // shared with non-id selectors (forHandle, channelId, …); those must
        // pass straight through to the inner client.
        if !url.query_pairs().any(|(k, _)| k == "id") {
            return None;
        }

        let mut hasher = DefaultHasher::new();
        API_HOST.hash(&mut hasher);
        self.path.hash(&mut hasher);

        // Headers — include so requests with different API keys never merge.
        let mut headers: Vec<&(HeaderName, HeaderValue)> = req.headers.iter().collect();
        headers.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        for (k, v) in &headers {
            k.as_str().hash(&mut hasher);
            v.as_bytes().hash(&mut hasher);
        }

        // Query params other than `id`, sorted for stability.
        let mut params: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(k, _)| k != "id")
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
        MAX_IDS_PER_CALL
    }

    fn merge(&self, reqs: &[Request]) -> Request {
        let template = &reqs[0];
        let mut url = reqwest::Url::parse(&template.url)
            .expect("group() only accepts parseable URLs; merge() can't see one that isn't");

        let ids: Vec<String> = reqs.iter().filter_map(Self::extract_id).collect();
        let preserved: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(k, _)| k != "id")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

        url.query_pairs_mut().clear();
        for (k, v) in &preserved {
            url.query_pairs_mut().append_pair(k, v);
        }
        url.query_pairs_mut().append_pair("id", &ids.join(","));

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
        // Failures: hand every waiter the same status + body. The caller's
        // per-request handling (e.g. `Error::NotFound`) still fires correctly
        // because the body shape on error is opaque to us.
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
        struct ListBody {
            #[serde(default)]
            items: Vec<Box<serde_json::value::RawValue>>,
            #[serde(flatten)]
            meta: serde_json::Map<String, serde_json::Value>,
        }

        #[derive(serde::Serialize)]
        struct SynthBody<'a> {
            #[serde(flatten)]
            meta: &'a serde_json::Map<String, serde_json::Value>,
            items: [&'a serde_json::value::RawValue; 1],
        }

        #[derive(serde::Deserialize)]
        struct ItemId {
            id: String,
        }

        let parsed: ListBody = serde_json::from_slice(merged_body)
            .map_err(|e| Arc::new(Error::from(BodyExtractError::Json(e))))?;

        // id → raw item lookup. Duplicate ids across waiters resolve to the same bytes.
        let by_id: HashMap<String, Box<serde_json::value::RawValue>> = parsed
            .items
            .into_iter()
            .filter_map(|item| {
                let id: ItemId = serde_json::from_str(item.get()).ok()?;
                Some((id.id, item))
            })
            .collect();

        let mut out = Vec::with_capacity(reqs.len());
        for req in reqs {
            let Some(id) = Self::extract_id(req) else {
                out.push(None);
                continue;
            };
            match by_id.get(&id) {
                Some(item) => {
                    let body = serde_json::to_vec(&SynthBody {
                        meta: &parsed.meta,
                        items: [item.as_ref()],
                    })
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

    use super::{YouTubeListRule, *};
    use crate::http::{Method, Request, ResponseStatus};

    fn req(id: &str) -> Request {
        Request {
            method: Method::GET,
            url: format!(
                "https://www.googleapis.com/youtube/v3/videos?part=snippet,contentDetails&id={id}"
            ),
            ..Default::default()
        }
    }

    #[test]
    fn group_accepts_videos_endpoint() {
        let rule = YouTubeListRule::videos();
        assert!(rule.group(&req("ABC")).is_some());
    }

    #[test]
    fn group_rejects_other_endpoints() {
        let rule = YouTubeListRule::videos();
        let r = Request {
            method: Method::GET,
            url: "https://www.googleapis.com/youtube/v3/channels?id=X".into(),
            ..Default::default()
        };
        // The videos rule must not own the channels path.
        assert!(rule.group(&r).is_none());
    }

    #[test]
    fn group_rejects_other_host() {
        let rule = YouTubeListRule::videos();
        let r = Request {
            method: Method::GET,
            url: "https://example.com/youtube/v3/videos?id=X".into(),
            ..Default::default()
        };
        assert!(rule.group(&r).is_none());
    }

    #[test]
    fn channels_rule_accepts_id_based_requests() {
        let rule = YouTubeListRule::channels();
        let r = Request {
            method: Method::GET,
            url: "https://www.googleapis.com/youtube/v3/channels?part=snippet,contentDetails&id=UC123"
                .into(),
            ..Default::default()
        };
        assert!(rule.group(&r).is_some());
    }

    #[test]
    fn channels_rule_rejects_non_id_selectors() {
        let rule = YouTubeListRule::channels();
        // forHandle / forUsername variants carry no `id` → not batchable.
        for url in [
            "https://www.googleapis.com/youtube/v3/channels?part=snippet&forHandle=foo",
            "https://www.googleapis.com/youtube/v3/channels?part=snippet&forUsername=foo",
        ] {
            let r = Request {
                method: Method::GET,
                url: url.into(),
                ..Default::default()
            };
            assert!(rule.group(&r).is_none(), "should not batch: {url}");
        }
    }

    #[test]
    fn playlists_rule_accepts_id_but_rejects_channelid() {
        let rule = YouTubeListRule::playlists();
        let by_id = Request {
            method: Method::GET,
            url: "https://www.googleapis.com/youtube/v3/playlists?part=snippet&id=PL123".into(),
            ..Default::default()
        };
        assert!(rule.group(&by_id).is_some());

        // The channel-owned-playlists query selects by channelId, not id.
        let by_channel = Request {
            method: Method::GET,
            url: "https://www.googleapis.com/youtube/v3/playlists?part=snippet&channelId=UC123&maxResults=50"
                .into(),
            ..Default::default()
        };
        assert!(rule.group(&by_channel).is_none());
    }

    #[test]
    fn group_keys_equal_for_same_part_param() {
        let rule = YouTubeListRule::videos();
        let a = rule.group(&req("AAA")).unwrap();
        let b = rule.group(&req("BBB")).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn group_keys_differ_when_part_differs() {
        let rule = YouTubeListRule::videos();
        let a = rule.group(&req("AAA")).unwrap();
        let b_req = Request {
            method: Method::GET,
            url: "https://www.googleapis.com/youtube/v3/videos?part=statistics&id=BBB".into(),
            ..Default::default()
        };
        let b = rule.group(&b_req).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn group_keys_differ_across_endpoints() {
        // Even with identical query params, two rules for different endpoints
        // produce distinct keys (different path mixed into the hash).
        let videos = YouTubeListRule::videos();
        let channels = YouTubeListRule::channels();
        let v = videos
            .group(&Request {
                method: Method::GET,
                url: "https://www.googleapis.com/youtube/v3/videos?part=snippet&id=X".into(),
                ..Default::default()
            })
            .unwrap();
        let c = channels
            .group(&Request {
                method: Method::GET,
                url: "https://www.googleapis.com/youtube/v3/channels?part=snippet&id=X".into(),
                ..Default::default()
            })
            .unwrap();
        assert_ne!(v, c);
    }

    #[test]
    fn merge_collects_ids_and_preserves_other_params() {
        let rule = YouTubeListRule::videos();
        let reqs = vec![req("A"), req("B"), req("C")];
        let merged = rule.merge(&reqs);
        let url = reqwest::Url::parse(&merged.url).unwrap();
        let id = url
            .query_pairs()
            .find(|(k, _)| k == "id")
            .map(|(_, v)| v.into_owned())
            .unwrap();
        assert_eq!(id, "A,B,C");
        let part = url
            .query_pairs()
            .find(|(k, _)| k == "part")
            .map(|(_, v)| v.into_owned())
            .unwrap();
        assert_eq!(part, "snippet,contentDetails");
    }

    #[test]
    fn split_maps_items_by_id_and_synthesises_single_item_bodies() {
        let rule = YouTubeListRule::videos();
        let body = serde_json::json!({
            "kind": "youtube#videoListResponse",
            "etag": "etag-merged",
            "items": [
                { "kind": "youtube#video", "id": "A", "snippet": { "title": "alpha" } },
                { "kind": "youtube#video", "id": "C", "snippet": { "title": "gamma" } },
                // No B — simulating a missing/private video.
            ],
            "pageInfo": { "totalResults": 3, "resultsPerPage": 50 }
        });
        let merged_body = Bytes::from(serde_json::to_vec(&body).unwrap());
        let reqs = vec![req("A"), req("B"), req("C")];

        let splits = rule
            .split(ResponseStatus::OK, &[], &merged_body, &reqs)
            .unwrap();
        assert_eq!(splits.len(), 3);

        let a = splits[0].as_ref().unwrap();
        let a_json: serde_json::Value = serde_json::from_slice(&a.body).unwrap();
        assert_eq!(a_json["items"].as_array().unwrap().len(), 1);
        assert_eq!(a_json["items"][0]["id"], "A");

        // B was missing from the merged response.
        assert!(splits[1].is_none());

        let c = splits[2].as_ref().unwrap();
        let c_json: serde_json::Value = serde_json::from_slice(&c.body).unwrap();
        assert_eq!(c_json["items"][0]["id"], "C");
        // Meta preserved.
        assert_eq!(c_json["kind"], "youtube#videoListResponse");
    }

    #[test]
    fn split_on_failure_propagates_status_to_all_waiters() {
        let rule = YouTubeListRule::videos();
        let merged_body = Bytes::from_static(b"{\"error\":{\"code\":403}}");
        let reqs = vec![req("A"), req("B")];
        let splits = rule
            .split(ResponseStatus::FORBIDDEN, &[], &merged_body, &reqs)
            .unwrap();
        for s in splits {
            let s = s.unwrap();
            assert_eq!(s.status, ResponseStatus::FORBIDDEN);
            assert!(!s.body.is_empty());
        }
    }
}
