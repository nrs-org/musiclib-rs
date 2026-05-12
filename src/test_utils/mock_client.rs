use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use serde::{Serialize, de::DeserializeOwned};

use crate::http::{HeaderValue, HttpClient, Method, Response, ResponseBody, ResponseStatus};

pub struct MockHttpClient {
    routes: HashMap<(Method, String), Arc<Response>>,
    on_route_callback: Option<Arc<dyn Fn(&Method, &str) + Send + Sync>>,
}

impl MockHttpClient {
    pub fn new() -> Self {
        Self {
            routes: HashMap::new(),
            on_route_callback: None,
        }
    }

    pub fn add_route(&mut self, method: Method, url: &str, response: Arc<Response>) {
        self.routes.insert((method, url.to_string()), response);
    }

    pub fn add_route_json<T>(&mut self, method: Method, url: &str, content: &'static str)
    where
        T: DeserializeOwned + Serialize + Send + Sync + 'static,
    {
        let value = serde_json::from_str::<T>(content).expect("Invalid JSON content");
        self.add_route(
            method,
            url,
            Arc::new(Response {
                status: ResponseStatus::OK,
                headers: vec![(
                    http::header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                )],
                body: ResponseBody::from_json(value),
            }),
        );
    }

    pub fn set_on_route_callback<F>(&mut self, callback: F)
    where
        F: Fn(&Method, &str) + Send + Sync + 'static,
    {
        self.on_route_callback = Some(Arc::new(callback));
    }

    fn not_found_response(&self) -> Arc<Response> {
        Arc::new(Response {
            status: ResponseStatus::NOT_FOUND,
            headers: vec![],
            body: ResponseBody::from(vec![]),
        })
    }
}

#[async_trait]
impl HttpClient for MockHttpClient {
    async fn make_request(
        &self,
        req: crate::http::Request,
        _body_extractor: &dyn crate::http::BodyExtractor,
    ) -> Result<Arc<Response>, crate::http::Error> {
        if let Some(callback) = &self.on_route_callback {
            callback(&req.method, &req.url);
        }
        if let Some(res) = self.routes.get(&(req.method, req.url)).cloned() {
            return Ok(res);
        }
        Ok(self.not_found_response())
    }
}
