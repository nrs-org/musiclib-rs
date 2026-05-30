use http_body_util::Full;
use hyper::{Request, Response, body::Bytes, service::service_fn};
use hyper_util::rt::TokioIo;
use std::{net::SocketAddr, sync::Arc};
use tokio::{net::TcpListener, select};
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::error;

pub struct MockServer {
    _guard: DropGuard,
    addr: SocketAddr,
}

impl MockServer {
    const DEFAULT_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(5);

    pub async fn new<F, S>(handler: F) -> anyhow::Result<Self>
    where
        F: Fn(Request<hyper::body::Incoming>) -> S + Send + Sync + 'static,
        S: Future<Output = anyhow::Result<Response<Full<Bytes>>>> + Send,
    {
        Self::new_with_timeout(handler, Self::DEFAULT_TIMEOUT).await
    }

    pub async fn new_with_timeout<F, S>(
        handler: F,
        timeout: tokio::time::Duration,
    ) -> anyhow::Result<Self>
    where
        F: Fn(Request<hyper::body::Incoming>) -> S + Send + Sync + 'static,
        S: Future<Output = anyhow::Result<Response<Full<Bytes>>>> + Send,
    {
        let (guard, addr) = Self::launch_mock_server(handler, timeout).await?;
        Ok(Self {
            _guard: guard,
            addr,
        })
    }

    pub fn route(&self, route: &str) -> String {
        format!("http://{}{}", self.addr, route)
    }

    async fn launch_mock_server<F, S>(
        handler: F,
        timeout: tokio::time::Duration,
    ) -> anyhow::Result<(DropGuard, SocketAddr)>
    where
        F: Fn(Request<hyper::body::Incoming>) -> S + Send + Sync + 'static,
        S: Future<Output = anyhow::Result<Response<Full<Bytes>>>> + Send,
    {
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let listener = TcpListener::bind(addr).await?;
        let local_addr = listener.local_addr()?;

        let cancel_token = CancellationToken::new();

        let task_token = cancel_token.clone();
        let handler = Arc::new(handler);
        tokio::task::spawn(async move {
            loop {
                select! {
                     _ = task_token.cancelled() => {
                        break
                    },
                    _ = tokio::time::sleep(timeout) => {
                        break
                    },
                    connection = listener.accept() => {
                        let (stream, _) = connection.unwrap();
                        let io = TokioIo::new(stream);
                        let handler_ref = handler.clone();
                        tokio::task::spawn(async move {
                            if let Err(err) = hyper::server::conn::http1::Builder::new()
                                .serve_connection(io, service_fn(handler_ref.as_ref()))
                                .await
                            {
                                error!("Error serving connection: {:?}", err);
                            }
                        });
                    }
                }
            }
        });

        let guard = cancel_token.drop_guard();
        Ok((guard, local_addr))
    }
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{MockServer, init_test_logger};
    use http_body_util::Full;
    use hyper::{Response, body::Bytes};

    #[tokio::test]
    async fn test_mock_server() -> anyhow::Result<()> {
        init_test_logger();
        let server = MockServer::new(|_| async {
            Ok(Response::new(Full::new(Bytes::from("Hello, World!"))))
        })
        .await?;

        let res = reqwest::get(server.route("/")).await?;
        let text = res.text().await?;
        assert_eq!(text, "Hello, World!");
        Ok(())
    }

    #[tokio::test]
    async fn test_mock_server_timeout() -> anyhow::Result<()> {
        init_test_logger();
        let server = MockServer::new_with_timeout(
            |_| async { Ok(Response::new(Full::new(Bytes::from("Hello, World!")))) },
            std::time::Duration::from_secs(1),
        )
        .await?;

        let res = reqwest::get(server.route("/")).await?;
        let text = res.text().await?;
        assert_eq!(text, "Hello, World!");

        // Wait for the server to timeout
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // The server should have stopped, so this should fail,
        // added client-side timeout to ensure the test doesn't hang indefinitely
        // on weird platforms.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(1))
            .build()?;
        let res = client.get(server.route("/")).send().await;
        assert!(res.is_err());
        Ok(())
    }
}
