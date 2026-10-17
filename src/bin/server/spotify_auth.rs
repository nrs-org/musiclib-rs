//! The player's Spotify login, held by the server instead of each browser:
//! one Client ID and refresh token for every device, so connecting once
//! covers them all.
//!
//! The browser still runs the PKCE redirect (the Spotify app's registered
//! redirect URI points at the player) and hands the authorization code
//! here; the code exchange, the refresh token and its rotation stay on
//! this side. Devices ask `GET /api/spotify/token` for a short-lived access
//! token, which the Web Playback SDK and the Web API calls use as before.
//!
//! Saved in `spotify.json` next to the DB, like `playback.json`, readable
//! by the owner only. Only the Client ID and refresh token are written;
//! the access token lives in memory. Refreshes run one at a time under
//! the lock: Spotify may rotate the refresh token, so two concurrent
//! refreshes would race for it.

use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, anyhow};
use bytes::Bytes;
use hyper::{
    Method, StatusCode,
    header::{CACHE_CONTROL, HeaderValue},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::{
    respond::{self, HyperResponse},
    state::AppState,
};

const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
/// Refresh this long before expiry, so a token handed out has time to be used.
const REFRESH_MARGIN_MS: i64 = 60_000;

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
struct Saved {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
}

struct Access {
    token: String,
    /// Unix milliseconds.
    expires_at: i64,
}

#[derive(Default)]
struct Inner {
    saved: Saved,
    access: Option<Access>,
}

pub struct SpotifyAuth {
    path: PathBuf,
    token_url: String,
    client: reqwest::Client,
    inner: Mutex<Inner>,
}

/// Why Spotify's token endpoint gave no token.
#[derive(Debug)]
enum ExchangeError {
    /// Spotify refused the grant (a revoked or rotated-away refresh token,
    /// a reused code): the request won't work if repeated.
    Rejected(String),
    /// Spotify couldn't be reached or answered oddly; worth retrying.
    Upstream(anyhow::Error),
    /// No Client ID is set, so there's nothing to log in to.
    NoClientId,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: i64,
    refresh_token: Option<String>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

impl SpotifyAuth {
    pub fn load(path: PathBuf) -> Self {
        Self::with_token_url(path, TOKEN_URL.to_owned())
    }

    fn with_token_url(path: PathBuf, token_url: String) -> Self {
        let saved = std::fs::read(&path)
            .ok()
            .and_then(|b| match serde_json::from_slice(&b) {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!("ignoring unreadable {}: {e}", path.display());
                    None
                }
            })
            .unwrap_or_default();
        Self {
            path,
            token_url,
            client: reqwest::Client::new(),
            inner: Mutex::new(Inner {
                saved,
                access: None,
            }),
        }
    }

    async fn save(&self, saved: &Saved) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec_pretty(saved)?;
        let tmp = self.path.with_extension("json.tmp");
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&tmp)
            .await
            .with_context(|| format!("writing {}", tmp.display()))?;
        tokio::io::AsyncWriteExt::write_all(&mut file, &bytes).await?;
        file.sync_all().await?;
        tokio::fs::rename(&tmp, &self.path)
            .await
            .with_context(|| format!("saving {}", self.path.display()))
    }

    async fn exchange(&self, params: &[(&str, &str)]) -> Result<TokenResponse, ExchangeError> {
        let body = params
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        let res = self
            .client
            .post(&self.token_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|e| ExchangeError::Upstream(anyhow!(e).context("reaching Spotify")))?;
        let status = res.status().as_u16();
        let text = res
            .text()
            .await
            .map_err(|e| ExchangeError::Upstream(e.into()))?;
        if status == 400 || status == 401 {
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let message = v["error_description"]
                .as_str()
                .or(v["error"].as_str())
                .unwrap_or(&text);
            return Err(ExchangeError::Rejected(message.to_owned()));
        }
        if !(200..300).contains(&status) {
            return Err(ExchangeError::Upstream(anyhow!(
                "Spotify token endpoint answered {status}"
            )));
        }
        serde_json::from_str(&text).map_err(|e| {
            ExchangeError::Upstream(anyhow!(e).context("reading Spotify's token response"))
        })
    }

    /// Takes a token response: the access token into memory, and a rotated
    /// refresh token (if Spotify sent one) into `inner` and onto disk.
    async fn accept(&self, inner: &mut Inner, res: TokenResponse) -> anyhow::Result<()> {
        inner.access = Some(Access {
            token: res.access_token,
            expires_at: now_ms() + res.expires_in * 1000,
        });
        if let Some(refresh) = res.refresh_token
            && inner.saved.refresh_token.as_deref() != Some(refresh.as_str())
        {
            inner.saved.refresh_token = Some(refresh);
            self.save(&inner.saved).await?;
        }
        Ok(())
    }

    async fn status(&self) -> Value {
        let inner = self.inner.lock().await;
        json!({
            "client_id": inner.saved.client_id.as_deref().unwrap_or(""),
            "connected": inner.saved.refresh_token.is_some(),
        })
    }

    /// A different Client ID drops the login: its tokens belong to the old app.
    async fn set_client_id(&self, client_id: &str) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().await;
        if inner.saved.client_id.as_deref() == Some(client_id) {
            return Ok(());
        }
        inner.saved = Saved {
            client_id: Some(client_id.to_owned()),
            refresh_token: None,
        };
        inner.access = None;
        self.save(&inner.saved).await
    }

    /// Finishes a browser's PKCE login: trades its code for tokens.
    async fn connect(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<(), ExchangeError> {
        let mut inner = self.inner.lock().await;
        let Some(client_id) = inner.saved.client_id.clone() else {
            return Err(ExchangeError::NoClientId);
        };
        let res = self
            .exchange(&[
                ("client_id", &client_id),
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("code_verifier", verifier),
            ])
            .await?;
        if res.refresh_token.is_none() {
            return Err(ExchangeError::Upstream(anyhow!(
                "Spotify sent no refresh token"
            )));
        }
        self.accept(&mut inner, res)
            .await
            .map_err(ExchangeError::Upstream)
    }

    /// Adopts a login a browser held before this module existed, if
    /// Spotify still honors its refresh token.
    async fn import(&self, client_id: &str, refresh_token: &str) -> Result<(), ExchangeError> {
        let mut inner = self.inner.lock().await;
        let res = self
            .exchange(&[
                ("client_id", client_id),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
            ])
            .await?;
        inner.saved = Saved {
            client_id: Some(client_id.to_owned()),
            refresh_token: Some(refresh_token.to_owned()),
        };
        self.save(&inner.saved)
            .await
            .map_err(ExchangeError::Upstream)?;
        self.accept(&mut inner, res)
            .await
            .map_err(ExchangeError::Upstream)
    }

    /// A current access token and its expiry (unix ms), refreshed when
    /// close to expiring. `None` when there's no login, including one
    /// Spotify just refused (which is dropped).
    async fn access_token(&self) -> Result<Option<(String, i64)>, anyhow::Error> {
        let mut inner = self.inner.lock().await;
        if let Some(a) = &inner.access
            && a.expires_at - now_ms() > REFRESH_MARGIN_MS
        {
            return Ok(Some((a.token.clone(), a.expires_at)));
        }
        let (Some(client_id), Some(refresh)) = (
            inner.saved.client_id.clone(),
            inner.saved.refresh_token.clone(),
        ) else {
            return Ok(None);
        };
        match self
            .exchange(&[
                ("client_id", &client_id),
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh),
            ])
            .await
        {
            Ok(res) => {
                self.accept(&mut inner, res).await?;
                let a = inner.access.as_ref().expect("accept sets access");
                Ok(Some((a.token.clone(), a.expires_at)))
            }
            Err(ExchangeError::Rejected(message)) => {
                tracing::warn!("Spotify refused the refresh token ({message}); disconnecting");
                inner.saved.refresh_token = None;
                inner.access = None;
                self.save(&inner.saved).await?;
                Ok(None)
            }
            Err(ExchangeError::Upstream(e)) => Err(e),
            Err(ExchangeError::NoClientId) => Ok(None),
        }
    }

    async fn disconnect(&self) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().await;
        inner.saved.refresh_token = None;
        inner.access = None;
        self.save(&inner.saved).await
    }
}

pub fn handles(path: &str) -> bool {
    path.starts_with("/api/spotify/")
}

#[derive(Deserialize)]
struct ClientRequest {
    client_id: String,
}

#[derive(Deserialize)]
struct ConnectRequest {
    code: String,
    code_verifier: String,
    redirect_uri: String,
}

#[derive(Deserialize)]
struct ImportRequest {
    client_id: String,
    refresh_token: String,
}

fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, HyperResponse> {
    serde_json::from_slice(body)
        .map_err(|e| respond::error(StatusCode::BAD_REQUEST, "invalid_input", &e.to_string()))
}

fn internal(e: anyhow::Error) -> HyperResponse {
    respond::error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        &format!("{e:#}"),
    )
}

fn exchange_error(e: ExchangeError) -> HyperResponse {
    match e {
        ExchangeError::Rejected(message) => respond::error(
            StatusCode::BAD_REQUEST,
            "spotify_rejected",
            &format!("Spotify refused: {message}"),
        ),
        ExchangeError::Upstream(e) => {
            respond::error(StatusCode::BAD_GATEWAY, "upstream", &format!("{e:#}"))
        }
        ExchangeError::NoClientId => respond::error(
            StatusCode::CONFLICT,
            "not_configured",
            "set a Spotify Client ID first",
        ),
    }
}

/// - `GET status`: `{client_id, connected}`, never a token.
/// - `PUT client` `{client_id}`: the Spotify app to log in with.
/// - `POST connect` `{code, code_verifier, redirect_uri}`: a finished
///   PKCE redirect.
/// - `POST import` `{client_id, refresh_token}`: a pre-server login.
/// - `GET token`: `{access_token, expires_at}`, or 409 `not_connected`.
/// - `DELETE token`: log out, on every device.
pub async fn route(state: &AppState, method: &Method, path: &str, body: Bytes) -> HyperResponse {
    let auth = &state.spotify;
    let ok = || respond::json(StatusCode::OK, json!({ "ok": true }));
    match (method, &path["/api/spotify/".len()..]) {
        (&Method::GET, "status") => respond::json(StatusCode::OK, auth.status().await),
        (&Method::PUT, "client") => {
            let req: ClientRequest = match parse(&body) {
                Ok(r) => r,
                Err(resp) => return resp,
            };
            let id = req.client_id.trim();
            if id.is_empty() {
                return respond::error(StatusCode::BAD_REQUEST, "invalid_input", "empty client_id");
            }
            match auth.set_client_id(id).await {
                Ok(()) => ok(),
                Err(e) => internal(e),
            }
        }
        (&Method::POST, "connect") => {
            let req: ConnectRequest = match parse(&body) {
                Ok(r) => r,
                Err(resp) => return resp,
            };
            match auth
                .connect(&req.code, &req.code_verifier, &req.redirect_uri)
                .await
            {
                Ok(()) => ok(),
                Err(e) => exchange_error(e),
            }
        }
        (&Method::POST, "import") => {
            let req: ImportRequest = match parse(&body) {
                Ok(r) => r,
                Err(resp) => return resp,
            };
            match auth.import(req.client_id.trim(), &req.refresh_token).await {
                Ok(()) => ok(),
                Err(e) => exchange_error(e),
            }
        }
        (&Method::GET, "token") => match auth.access_token().await {
            Ok(Some((access_token, expires_at))) => {
                let mut resp = respond::json(
                    StatusCode::OK,
                    json!({ "access_token": access_token, "expires_at": expires_at }),
                );
                resp.headers_mut()
                    .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
                resp
            }
            Ok(None) => respond::error(
                StatusCode::CONFLICT,
                "not_connected",
                "Spotify isn't connected",
            ),
            Err(e) => respond::error(StatusCode::BAD_GATEWAY, "upstream", &format!("{e:#}")),
        },
        (&Method::DELETE, "token") => match auth.disconnect().await {
            Ok(()) => ok(),
            Err(e) => internal(e),
        },
        (_, "status" | "client" | "connect" | "import" | "token") => respond::method_not_allowed(),
        _ => respond::not_found(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;

    /// A stand-in for Spotify's token endpoint: answers each request with
    /// the next canned `(status, body)`, recording the request bodies.
    async fn fake_spotify(
        replies: Vec<(u16, &'static str)>,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api/token", listener.local_addr().unwrap());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let count = Arc::new(AtomicUsize::new(0));
        let (seen2, count2) = (Arc::clone(&seen), Arc::clone(&count));
        tokio::spawn(async move {
            for (status, body) in replies {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = sock.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf);
                    if let Some(head_end) = text.find("\r\n\r\n") {
                        let len = text[..head_end]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if buf.len() >= head_end + 4 + len {
                            seen2.lock().unwrap().push(text[head_end + 4..].to_string());
                            break;
                        }
                    }
                }
                count2.fetch_add(1, Ordering::SeqCst);
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        (url, seen, count)
    }

    fn saved_on_disk(path: &std::path::Path) -> Saved {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn refresh_keeps_a_rotated_token_and_reuses_the_access_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spotify.json");
        let (url, seen, count) = fake_spotify(vec![(
            200,
            r#"{"access_token":"A1","expires_in":3600,"refresh_token":"R2"}"#,
        )])
        .await;
        let auth = SpotifyAuth::with_token_url(path.clone(), url);
        auth.set_client_id("cid").await.unwrap();
        {
            let mut inner = auth.inner.lock().await;
            inner.saved.refresh_token = Some("R1".into());
        }

        let (token, _) = auth.access_token().await.unwrap().unwrap();
        assert_eq!(token, "A1");
        assert!(seen.lock().unwrap()[0].contains("refresh_token=R1"));
        assert_eq!(
            saved_on_disk(&path),
            Saved {
                client_id: Some("cid".into()),
                refresh_token: Some("R2".into()),
            }
        );
        // Still fresh: served from memory, no second request.
        let (again, _) = auth.access_token().await.unwrap().unwrap();
        assert_eq!(again, "A1");
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_refused_refresh_token_disconnects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spotify.json");
        let (url, _, _) = fake_spotify(vec![(
            400,
            r#"{"error":"invalid_grant","error_description":"Refresh token revoked"}"#,
        )])
        .await;
        std::fs::write(&path, r#"{"client_id":"cid","refresh_token":"R1"}"#).unwrap();
        let auth = SpotifyAuth::with_token_url(path.clone(), url);

        assert!(auth.access_token().await.unwrap().is_none());
        assert_eq!(auth.status().await["connected"], false);
        assert_eq!(saved_on_disk(&path).refresh_token, None);
        assert_eq!(saved_on_disk(&path).client_id.as_deref(), Some("cid"));
    }

    #[tokio::test]
    async fn an_unreachable_spotify_keeps_the_login() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spotify.json");
        let (url, _, _) = fake_spotify(vec![(503, "{}")]).await;
        std::fs::write(&path, r#"{"client_id":"cid","refresh_token":"R1"}"#).unwrap();
        let auth = SpotifyAuth::with_token_url(path.clone(), url);

        assert!(auth.access_token().await.is_err());
        assert_eq!(auth.status().await["connected"], true);
        assert_eq!(saved_on_disk(&path).refresh_token.as_deref(), Some("R1"));
    }

    #[tokio::test]
    async fn connect_exchanges_the_code_and_saves_privately() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spotify.json");
        let (url, seen, _) = fake_spotify(vec![(
            200,
            r#"{"access_token":"A1","expires_in":3600,"refresh_token":"R1"}"#,
        )])
        .await;
        let auth = SpotifyAuth::with_token_url(path.clone(), url);
        auth.set_client_id("cid").await.unwrap();

        auth.connect("CODE", "VERIFIER", "http://127.0.0.1:4600/")
            .await
            .unwrap();
        let body = seen.lock().unwrap()[0].clone();
        for part in [
            "grant_type=authorization_code",
            "code=CODE",
            "code_verifier=VERIFIER",
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A4600%2F",
            "client_id=cid",
        ] {
            assert!(body.contains(part), "{part} missing from {body}");
        }
        assert_eq!(saved_on_disk(&path).refresh_token.as_deref(), Some("R1"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // The status a device sees never carries a token.
        let status = auth.status().await.to_string();
        assert!(!status.contains("R1") && !status.contains("A1"));
    }

    #[tokio::test]
    async fn import_adopts_only_a_login_spotify_still_honors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spotify.json");
        let (url, _, _) = fake_spotify(vec![
            (400, r#"{"error":"invalid_grant"}"#),
            (200, r#"{"access_token":"A1","expires_in":3600}"#),
        ])
        .await;
        let auth = SpotifyAuth::with_token_url(path.clone(), url);

        assert!(matches!(
            auth.import("cid", "STALE").await,
            Err(ExchangeError::Rejected(_))
        ));
        assert_eq!(auth.status().await["connected"], false);
        assert!(!path.exists());

        auth.import("cid", "R1").await.unwrap();
        assert_eq!(
            saved_on_disk(&path),
            Saved {
                client_id: Some("cid".into()),
                refresh_token: Some("R1".into()),
            }
        );
        let (token, _) = auth.access_token().await.unwrap().unwrap();
        assert_eq!(token, "A1");
    }

    #[tokio::test]
    async fn a_new_client_id_drops_the_login() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spotify.json");
        std::fs::write(&path, r#"{"client_id":"old","refresh_token":"R1"}"#).unwrap();
        let auth = SpotifyAuth::load(path.clone());

        auth.set_client_id("old").await.unwrap();
        assert_eq!(auth.status().await["connected"], true);
        auth.set_client_id("new").await.unwrap();
        assert_eq!(auth.status().await["connected"], false);
        assert_eq!(saved_on_disk(&path).client_id.as_deref(), Some("new"));
    }
}
