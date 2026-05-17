use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use serde::de::DeserializeOwned;

use crate::http::{
    BodyExtractor, HeaderValue, HttpClient, Method, Request, Response, ResponseBody, ResponseStatus,
};

/// Raw response stored in the mock before the body extractor is applied.
#[derive(Clone)]
struct RawRoute {
    status: ResponseStatus,
    headers: Vec<(http::header::HeaderName, HeaderValue)>,
    body: Vec<u8>,
}

impl RawRoute {
    fn json(status: ResponseStatus, body: Vec<u8>) -> Self {
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

    pub fn add_route(&mut self, method: Method, url: &str, status: ResponseStatus, body: Vec<u8>) {
        self.push_route(method, url, RawRoute::json(status, body));
    }

    pub fn add_route_json<T>(&mut self, method: Method, url: &str, content: &'static str)
    where
        T: DeserializeOwned + 'static,
    {
        serde_json::from_str::<T>(content).expect("Invalid JSON content");
        self.add_route(method, url, ResponseStatus::OK, content.as_bytes().to_vec());
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
        body_extractor: &dyn BodyExtractor,
    ) -> Result<Arc<Response>, crate::http::Error> {
        if let Some(callback) = &self.on_route_callback {
            callback(&req.method, &req.url);
        }

        let queue = match self.routes.get(&(req.method, req.url)) {
            Some(q) => q,
            None => {
                return Ok(Arc::new(Response {
                    status: ResponseStatus::NOT_FOUND,
                    headers: vec![],
                    body: ResponseBody::from(vec![]),
                }));
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

        let reqwest_resp = http::Response::builder()
            .status(raw.status)
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(raw.body)
            .unwrap();
        let reqwest_resp = reqwest::Response::from(reqwest_resp);

        let body = body_extractor
            .extract(reqwest_resp)
            .await
            .map_err(crate::http::Error::BodyExtract)?;

        Ok(Arc::new(Response {
            status: raw.status,
            headers: raw.headers,
            body,
        }))
    }
}
