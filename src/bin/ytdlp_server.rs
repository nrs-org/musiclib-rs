/// HTTP server implementing the fetch half of the yt-dlp backend.
/// Given a canonical ID, invokes yt-dlp and returns its raw JSON output.
/// URL canonicalization and EntityResult conversion are done in musiclib-rs.
///
/// Endpoints:
///   GET /entities/:id   — fetch raw yt-dlp JSON for the given ID
///
/// Env vars:
///   YTDLP_PATH           — path to the yt-dlp executable (default: "yt-dlp")
///   PROVIDER_SERVER_ADDR — listen address (default: "127.0.0.1:3000")
use std::{net::SocketAddr, sync::Arc};

use regex::Regex;

use http_body_util::Full;
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Bytes, Incoming},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::process::Command;

struct AppState {
    ytdlp_path: String,
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type HyperResponse = Response<Full<Bytes>>;

fn json_response(status: StatusCode, body: Value) -> HyperResponse {
    let bytes = Bytes::from(body.to_string());
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(Full::new(bytes))
        .expect("response builder")
}

/// Extract an HTTP status code from yt-dlp stderr lines like
/// `HTTP Error 404: Not Found` or `<HTTPError 404: Not Found>`.
fn http_status_from_stderr(stderr: &str) -> Option<StatusCode> {
    let re = Regex::new(r"HTTP\s*Error\s+(\d{3})").unwrap();
    let code: u16 = re
        .captures_iter(stderr)
        .last()?
        .get(1)?
        .as_str()
        .parse()
        .ok()?;
    StatusCode::from_u16(code).ok()
}

async fn handle(state: Arc<AppState>, req: Request<Incoming>) -> Result<HyperResponse, BoxError> {
    if req.method() != Method::GET {
        return Ok(json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            serde_json::json!({ "error": "method_not_allowed" }),
        ));
    }

    // Match /entities/:id
    let path = req.uri().path();
    let Some(id) = path.strip_prefix("/entities/").filter(|s| !s.is_empty()) else {
        return Ok(json_response(
            StatusCode::NOT_FOUND,
            serde_json::json!({ "error": "not_found" }),
        ));
    };
    let id = urlencoding::decode(id)?.into_owned();

    let query = req.uri().query().unwrap_or("");
    let full = query.split('&').any(|p| p == "full");
    let no_entries = query.split('&').any(|p| p == "no_entries");

    let mut args: Vec<&str> = vec![
        "--dump-single-json",
        "--flat-playlist",
        "--lazy-playlist",
        "--no-check-formats",
        "--extractor-retries",
        "0",
    ];
    if no_entries {
        args.extend_from_slice(&["--playlist-items", "0"]);
    }

    tracing::debug!(cmd = %state.ytdlp_path, args = ?args, id = %id, "running yt-dlp");
    let output = Command::new(&state.ytdlp_path)
        .args(&args)
        .arg(&id)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::error!("yt-dlp exited with {}: {stderr}", output.status);
        // Forward the upstream HTTP status when yt-dlp reports one explicitly.
        let status = http_status_from_stderr(&stderr).unwrap_or(StatusCode::BAD_GATEWAY);
        return Ok(json_response(
            status,
            serde_json::json!({
                "error": "upstream_error",
                "message": stderr.trim(),
            }),
        ));
    }

    let mut json: Value = serde_json::from_slice(&output.stdout)?;
    if !full && let Some(obj) = json.as_object_mut() {
        obj.remove("formats");
        obj.remove("requested_downloads");
    }
    Ok(json_response(StatusCode::OK, json))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt::init();

    let state = Arc::new(AppState {
        ytdlp_path: std::env::var("YTDLP_PATH").unwrap_or_else(|_| "yt-dlp".to_string()),
    });

    let addr: SocketAddr = std::env::var("PROVIDER_SERVER_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:3000".to_string())
        .parse()?;

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("ytdlp server listening on {addr}");

    loop {
        let (stream, _) = listener.accept().await?;
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            if let Err(e) = http1::Builder::new()
                .serve_connection(io, service_fn(move |req| handle(Arc::clone(&state), req)))
                .await
            {
                tracing::error!("connection error: {e}");
            }
        });
    }
}
