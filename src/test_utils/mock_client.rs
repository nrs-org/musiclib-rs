use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use bytes::Bytes;
use serde::de::DeserializeOwned;

use crate::http::{
    BodyExtractor, BodyExtractorCow, HeaderValue, HttpClient, Method, RawResponse, Request,
    Response, ResponseStatus,
};

/// Raw response stored in the mock before the body extractor is applied.
#[derive(Clone)]
struct RawRoute {
    status: ResponseStatus,
    headers: Vec<(http::header::HeaderName, HeaderValue)>,
    body: Bytes,
}

impl RawRoute {
    fn json(status: ResponseStatus, body: Bytes) -> Self {
        Self {
            status,
            headers: vec![(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )],
            body,
        }
    }
}

pub struct MockHttpClient {
    /// Each route holds a queue of responses. The last entry is never popped —
    /// once the queue reaches length 1 it is reused for all subsequent calls.
    routes: HashMap<(Method, String), Mutex<VecDeque<RawRoute>>>,
    on_route_callback: Option<Arc<dyn Fn(&Method, &str) + Send + Sync>>,
}

impl MockHttpClient {
    pub fn new() -> Self {
        Self {
            routes: HashMap::new(),
            on_route_callback: None,
        }
    }

    fn push_route(&mut self, method: Method, url: &str, raw: RawRoute) {
        self.routes
            .entry((method, url.to_string()))
            .or_insert_with(|| Mutex::new(VecDeque::new()))
            .lock()
            .unwrap()
            .push_back(raw);
    }

    pub fn add_route(&mut self, method: Method, url: &str, status: ResponseStatus, body: Bytes) {
        self.push_route(method, url, RawRoute::json(status, body));
    }

    pub fn add_route_json<T>(&mut self, method: Method, url: &str, content: &'static str)
    where
        T: DeserializeOwned + 'static,
    {
        serde_json::from_str::<T>(content).expect("Invalid JSON content");
        self.add_route(method, url, ResponseStatus::OK, content.into());
    }

    pub fn set_on_route_callback<F>(&mut self, callback: F)
    where
        F: Fn(&Method, &str) + Send + Sync + 'static,
    {
        self.on_route_callback = Some(Arc::new(callback));
    }
}

#[async_trait]
impl HttpClient for MockHttpClient {
    async fn make_request(
        &self,
        req: Request,
        body_extractor: BodyExtractorCow<'static>,
    ) -> Result<Arc<Response>, crate::http::Error> {
        if let Some(callback) = &self.on_route_callback {
            callback(&req.method, &req.url);
        }

        let queue = match self.routes.get(&(req.method, req.url)) {
            Some(q) => q,
            None => {
                return Ok(Arc::new(
                    body_extractor
                        .extract_response(RawResponse {
                            status: ResponseStatus::NOT_FOUND,
                            headers: Cow::Borrowed(&[]),
                            body: reqwest::Body::from(&[] as &[u8]),
                        })
                        .await?,
                ));
            }
        };

        let raw = {
            let mut guard = queue.lock().unwrap();
            // Keep the last entry so the route remains reusable indefinitely.
            if guard.len() > 1 {
                guard.pop_front().unwrap()
            } else {
                guard
                    .front()
                    .expect("route queue must not be empty")
                    .clone()
            }
        };

        let response = body_extractor
            .extract_response(RawResponse {
                status: raw.status,
                headers: Cow::Borrowed(&raw.headers),
                body: reqwest::Body::from(raw.body),
            })
            .await
            .map_err(crate::http::Error::from)?;

        Ok(Arc::new(response))
    }
}
