//! YouTube Data API quota counter.
//!
//! Counts outgoing requests to `https://www.googleapis.com/youtube/v3/*`. All
//! read endpoints used in this codebase (videos.list, channels.list,
//! playlists.list, playlistItems.list) cost 1 unit per call regardless of how
//! many IDs are passed, so the call count *is* the quota count.
//!
//! Positioned below the scheduler so retries are billed individually (matching
//! YouTube's accounting) and below the caches so cache hits don't count.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;

use crate::http::{BodyExtractorCow, Error, HttpClient, Request, Response};

const YOUTUBE_API_PREFIX: &str = "https://www.googleapis.com/youtube/v3/";

pub struct QuotaCounter {
    inner: Arc<dyn HttpClient>,
    youtube_count: Arc<AtomicU64>,
}

impl QuotaCounter {
    pub fn new(inner: Arc<dyn HttpClient>, youtube_count: Arc<AtomicU64>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            youtube_count,
        })
    }
}

#[async_trait]
impl HttpClient for QuotaCounter {
    async fn make_request(
        &self,
        req: Request,
        body_extractor: BodyExtractorCow<'static>,
    ) -> Result<Arc<Response>, Error> {
        if req.url.starts_with(YOUTUBE_API_PREFIX) {
            self.youtube_count.fetch_add(1, Ordering::Relaxed);
        }
        self.inner.make_request(req, body_extractor).await
    }
}

#[cfg(test)]
mod tests {
    use std::{
        borrow::Cow,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
    };

    use async_trait::async_trait;
    use bytes::Bytes;

    use super::QuotaCounter;
    use crate::http::{
        BodyExtractor, BodyExtractorCow, Error, HttpClient, Method, RawResponse, Request, Response,
        ResponseStatus,
    };

    struct OkClient;
    #[async_trait]
    impl HttpClient for OkClient {
        async fn make_request(
            &self,
            _req: Request,
            extractor: BodyExtractorCow<'static>,
        ) -> Result<Arc<Response>, Error> {
            let raw = RawResponse {
                status: ResponseStatus::OK,
                headers: Cow::Owned(Vec::new()),
                body: reqwest::Body::from(Bytes::from_static(b"")),
            };
            Ok(Arc::new(extractor.extract_response(raw).await?))
        }
    }

    fn req(url: &str) -> Request {
        Request {
            method: Method::GET,
            url: url.to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn counts_youtube_api_calls() {
        let count = Arc::new(AtomicU64::new(0));
        let counter: Arc<dyn HttpClient> =
            QuotaCounter::new(Arc::new(OkClient), Arc::clone(&count));

        counter
            .get_bytes(req(
                "https://www.googleapis.com/youtube/v3/videos?id=A&part=snippet",
            ))
            .await
            .unwrap();
        counter
            .get_bytes(req("https://www.googleapis.com/youtube/v3/channels?id=X"))
            .await
            .unwrap();

        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn ignores_non_youtube_calls() {
        let count = Arc::new(AtomicU64::new(0));
        let counter: Arc<dyn HttpClient> =
            QuotaCounter::new(Arc::new(OkClient), Arc::clone(&count));

        counter
            .get_bytes(req("https://api.spotify.com/v1/tracks/X"))
            .await
            .unwrap();
        counter
            .get_bytes(req("https://musicbrainz.org/ws/2/recording/X"))
            .await
            .unwrap();

        assert_eq!(count.load(Ordering::Relaxed), 0);
    }
}
