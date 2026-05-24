use std::sync::Arc;

use crate::http::{BodyExtractor, BodyExtractorCow};

use super::{HttpClient, Request, Response};
use async_trait::async_trait;

#[derive(Default)]
pub struct DefaultHttpClient {
    client: reqwest::Client,
}

impl From<reqwest::Client> for DefaultHttpClient {
    fn from(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl HttpClient for DefaultHttpClient {
    async fn make_request(
        &self,
        req: Request,
        body_extractor: BodyExtractorCow<'static>,
    ) -> Result<Arc<Response>, super::Error> {
        let res = self
            .client
            .request(req.method, &req.url)
            .headers(req.headers.iter().cloned().collect())
            .body(req.body.unwrap_or_default())
            .send()
            .await?;
        let res = Response::from(res, body_extractor)?;
        Ok(Arc::new(res))
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        http::{Method, Request, ResponseStatus, default_http_client},
        test_utils::{MockServer, init_test_logger},
    };
    use http_body_util::Full;
    use hyper::{Response as HyperResponse, body::Bytes};

    #[tokio::test]
    async fn test_get() -> anyhow::Result<()> {
        init_test_logger();
        let server = MockServer::new(async |_| {
            Ok(HyperResponse::new(Full::new(Bytes::from("Hello, World!"))))
        })
        .await?;

        let client = default_http_client();
        let req = Request {
            method: Method::GET,
            url: server.route("/"),
            ..Default::default()
        };

        let res = client.get(req).await?;
        assert_eq!(res.status, ResponseStatus::OK);
        assert_eq!(res.body.to_bytes()?, b"Hello, World!");

        Ok(())
    }

    #[tokio::test]
    async fn test_two_clients() -> anyhow::Result<()> {
        init_test_logger();
        let server = MockServer::new(async |_| {
            Ok(HyperResponse::new(Full::new(Bytes::from("Hello, World!"))))
        })
        .await?;

        let client1 = default_http_client();
        let client2 = default_http_client();
        let req = Request {
            method: Method::GET,
            url: server.route("/"),
            ..Default::default()
        };

        let res1 = client1.get(req.clone()).await?;
        let res2 = client2.get(req).await?;
        assert_eq!(res1.status, ResponseStatus::OK);
        assert_eq!(res1.body.to_bytes()?, b"Hello, World!");
        assert_eq!(res2.status, ResponseStatus::OK);
        assert_eq!(res2.body.to_bytes()?, b"Hello, World!");

        Ok(())
    }
}
