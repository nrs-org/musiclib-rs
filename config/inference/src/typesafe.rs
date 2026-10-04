//! TypeSafe System One client (feature `typesafe`): transport, retry,
//! concurrency and a response cache. It knows the `systemone` request and
//! response shape, but nothing about what the questions ask — the calling
//! script builds every request (state, model, questions) itself.
//!
//! ```text
//! h = inference_typesafe_open(config_json)          // NULL on error → inference_typesafe_last_error()
//!     // {"api_key": "…", "cache_path": "…"?, "concurrency": 8?, "base_url": "…"?,
//!     //  "timeout_s": 120?, "max_retries": 6?}
//! s = inference_typesafe_ask_batch(h, requests_json) // [request, …] → [result, …], same order
//!     // result: {"ok": <response>, "cached": bool} | {"error": "…"}
//!     inference_free_string(s); inference_typesafe_close(h)
//! ```
//!
//! A batch blocks until every request has an answer or has failed; requests
//! run on up to `concurrency` threads. 429, 5xx and transport errors are
//! retried (`Retry-After` when given, else exponential backoff) up to
//! `max_retries` times. Successful responses are cached by the SHA-256 of
//! the request's canonical (sorted-key) JSON, so any change to a prompt or
//! to the state sent is a new key. Errors are never cached.
//!
//! A request goes out with its object keys in the order the caller wrote
//! them (only whitespace is stripped): prompts are validated in a given
//! order, e.g. a task statement before its lists of cases.

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

pub const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai/v1/systemone";

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub api_key: String,
    #[serde(default)]
    pub cache_path: Option<String>,
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_timeout_s")]
    pub timeout_s: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

fn default_concurrency() -> usize {
    8
}
fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_owned()
}
fn default_timeout_s() -> u64 {
    120
}
fn default_max_retries() -> u32 {
    6
}

pub struct Client {
    config: Config,
    agent: ureq::Agent,
    cache: Option<Mutex<Connection>>,
}

/// One request's outcome: the response and whether it came from the cache.
pub type Answer = Result<(Value, bool), String>;

impl Client {
    pub fn open(config: Config) -> anyhow::Result<Self> {
        if config.api_key.is_empty() {
            anyhow::bail!("api_key is empty");
        }
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(config.timeout_s)))
            .http_status_as_error(false)
            .build()
            .into();
        let cache = match &config.cache_path {
            Some(path) => {
                if let Some(dir) = std::path::Path::new(path).parent() {
                    std::fs::create_dir_all(dir).ok();
                }
                let conn = Connection::open(path)
                    .with_context(|| format!("opening TypeSafe cache {path}"))?;
                conn.execute_batch(
                    "PRAGMA journal_mode = WAL;
                     CREATE TABLE IF NOT EXISTS response (
                         key        TEXT PRIMARY KEY,
                         request    TEXT NOT NULL,
                         response   TEXT NOT NULL,
                         created_at INTEGER NOT NULL
                     );",
                )
                .with_context(|| format!("initialising TypeSafe cache {path}"))?;
                Some(Mutex::new(conn))
            }
            None => None,
        };
        Ok(Self {
            config,
            agent,
            cache,
        })
    }

    /// Answers for every request (each a JSON object as text), in order.
    /// Cache lookups happen first; the misses then go out on up to
    /// `concurrency` threads.
    pub fn ask_batch(&self, requests: &[&str]) -> Vec<Answer> {
        let parsed: Vec<Result<Value, String>> = requests
            .iter()
            .map(|r| serde_json::from_str(r).map_err(|e| format!("request is not JSON: {e}")))
            .collect();
        let bodies: Vec<String> = requests.iter().map(|r| minify_json(r)).collect();
        let keys: Vec<String> = parsed
            .iter()
            .map(|v| {
                v.as_ref()
                    .map_or_else(|e| e.clone(), |v| sha256_hex(&canonical_json(v)))
            })
            .collect();
        let mut answers: Vec<Option<Answer>> = keys
            .iter()
            .zip(&parsed)
            .map(|(k, p)| match p {
                Err(e) => Some(Err(e.clone())),
                Ok(_) => self.cache_get(k).map(|v| Ok((v, true))),
            })
            .collect();

        // Identical requests (two pairs with the same evidence) go out once.
        let mut first_of: std::collections::HashMap<&str, usize> = Default::default();
        let misses: Vec<usize> = (0..requests.len())
            .filter(|&i| answers[i].is_none() && *first_of.entry(&keys[i]).or_insert(i) == i)
            .collect();
        let threads = self.config.concurrency.clamp(1, misses.len().max(1));
        let next = AtomicUsize::new(0);
        let fetched: Vec<(usize, Answer)> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..threads)
                .map(|_| {
                    let (next, misses, bodies, keys) = (&next, &misses, &bodies, &keys);
                    scope.spawn(move || {
                        let mut done = Vec::new();
                        while let Some(&i) = misses.get(next.fetch_add(1, Ordering::Relaxed)) {
                            let answer = self.post(&bodies[i]).map(|v| {
                                self.cache_put(&keys[i], &bodies[i], &v);
                                (v, false)
                            });
                            done.push((i, answer));
                        }
                        done
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|w| {
                    w.join()
                        .unwrap_or_else(|_| vec![(usize::MAX, Err("worker panicked".into()))])
                })
                .collect()
        });
        for (i, answer) in fetched {
            if let Some(slot) = answers.get_mut(i) {
                *slot = Some(answer);
            }
        }
        for i in 0..answers.len() {
            if answers[i].is_none()
                && let Some(&first) = first_of.get(keys[i].as_str())
            {
                answers[i] = answers[first].clone();
            }
        }
        answers
            .into_iter()
            .map(|a| a.unwrap_or_else(|| Err("request was not sent".into())))
            .collect()
    }

    fn post(&self, body: &str) -> Result<Value, String> {
        // A pooled keep-alive connection the server has since closed fails
        // at once with a transport error; retrying right away (on another
        // connection) fixes that without the backoff sleep.
        const QUICK_RETRIES: u32 = 3;
        let mut quick = 0u32;
        let mut attempt = 0u32;
        loop {
            let result = self
                .agent
                .post(&self.config.base_url)
                .header("Authorization", &format!("Bearer {}", self.config.api_key))
                .content_type("application/json")
                .send(body);
            let (retry_after, error) = match result {
                Ok(mut resp) => {
                    let status = resp.status().as_u16();
                    let text = resp.body_mut().read_to_string().unwrap_or_default();
                    if (200..300).contains(&status) {
                        return serde_json::from_str(&text).map_err(|e| {
                            format!("TypeSafe response is not JSON ({e}): {}", snippet(&text))
                        });
                    }
                    let error = format!("TypeSafe HTTP {status}: {}", snippet(&text));
                    if status != 429 && status < 500 {
                        return Err(error);
                    }
                    let retry_after = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<u64>().ok());
                    (retry_after, error)
                }
                Err(_) if quick < QUICK_RETRIES => {
                    quick += 1;
                    continue;
                }
                Err(e) => (None, format!("TypeSafe transport error: {e}")),
            };
            if attempt >= self.config.max_retries {
                return Err(format!("{error} (gave up after {} retries)", attempt));
            }
            let backoff = retry_after.unwrap_or(1u64 << attempt.min(6)).min(120);
            std::thread::sleep(Duration::from_secs(backoff));
            attempt += 1;
        }
    }

    fn cache_get(&self, key: &str) -> Option<Value> {
        let conn = self.cache.as_ref()?.lock().ok()?;
        let text: Option<String> = conn
            .prepare_cached("SELECT response FROM response WHERE key = ?1")
            .and_then(|mut st| st.query_row(params![key], |r| r.get(0)).optional())
            .ok()
            .flatten();
        text.and_then(|t| serde_json::from_str(&t).ok())
    }

    fn cache_put(&self, key: &str, request: &str, response: &Value) {
        let Some(conn) = self.cache.as_ref().and_then(|c| c.lock().ok()) else {
            return;
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        // A failed write only costs a repeat call later.
        let _ = conn.execute(
            "INSERT OR REPLACE INTO response (key, request, response, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![key, request, response.to_string(), now],
        );
    }
}

/// `text` without whitespace outside string literals, keeping key order.
fn minify_json(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let (mut in_string, mut escaped) = (false, false);
    for c in text.chars() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if !c.is_whitespace() {
            out.push(c);
        }
    }
    out
}

/// JSON with object keys sorted at every level, independent of whether
/// serde_json's `preserve_order` feature is on somewhere in the build.
fn canonical_json(v: &Value) -> String {
    fn sorted(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                let mut out = serde_json::Map::new();
                for k in keys {
                    out.insert(k.clone(), sorted(&m[k]));
                }
                Value::Object(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    // With `preserve_order` the map keeps insertion order (sorted above);
    // without it, it's a BTreeMap and sorted anyway.
    sorted(v).to_string()
}

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

fn snippet(s: &str) -> String {
    s.chars().take(300).collect()
}

// ── C ABI ───────────────────────────────────────────────────────────────────

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(CString::default());
}

fn set_error(msg: impl std::fmt::Display) {
    let s = CString::new(msg.to_string().replace('\0', " ")).unwrap_or_default();
    LAST_ERROR.with(|e| *e.borrow_mut() = s);
}

unsafe fn str_arg<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(p) }.to_str().ok()
}

/// Last error message on this thread ("" if none). Static until the next call.
#[unsafe(no_mangle)]
pub extern "C" fn inference_typesafe_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().as_ptr())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn inference_typesafe_open(config_json: *const c_char) -> *mut Client {
    let Some(text) = (unsafe { str_arg(config_json) }) else {
        set_error("inference_typesafe_open: config is null or not UTF-8");
        return std::ptr::null_mut();
    };
    let opened = serde_json::from_str::<Config>(text)
        .context("parsing TypeSafe client config")
        .and_then(Client::open);
    match opened {
        Ok(c) => Box::into_raw(Box::new(c)),
        Err(e) => {
            set_error(format!("{e:#}"));
            std::ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn inference_typesafe_close(h: *mut Client) {
    if !h.is_null() {
        drop(unsafe { Box::from_raw(h) });
    }
}

/// JSON array of results for a JSON array of requests (see the module docs);
/// NULL only when the input itself is unusable. Free with `inference_free_string`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn inference_typesafe_ask_batch(
    h: *const Client,
    requests_json: *const c_char,
) -> *mut c_char {
    let Some(client) = (unsafe { h.as_ref() }) else {
        set_error("inference_typesafe_ask_batch: null handle");
        return std::ptr::null_mut();
    };
    let Some(text) = (unsafe { str_arg(requests_json) }) else {
        set_error("inference_typesafe_ask_batch: requests are null or not UTF-8");
        return std::ptr::null_mut();
    };
    let requests: Vec<Box<serde_json::value::RawValue>> = match serde_json::from_str(text) {
        Ok(r) => r,
        Err(e) => {
            set_error(format!(
                "inference_typesafe_ask_batch: requests must be a JSON array: {e}"
            ));
            return std::ptr::null_mut();
        }
    };
    let requests: Vec<&str> = requests.iter().map(|r| r.get()).collect();
    let out: Vec<Value> = client
        .ask_batch(&requests)
        .into_iter()
        .map(|a| match a {
            Ok((v, cached)) => json!({"ok": v, "cached": cached}),
            Err(e) => json!({"error": e}),
        })
        .collect();
    CString::new(Value::Array(out).to_string())
        .unwrap_or_default()
        .into_raw()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::Arc;

    /// A tiny HTTP/1.1 server: answers each request with the next scripted
    /// `(status, body)` (the last one repeats), echoing the request body under
    /// `"echo"` on 200s. Returns its URL and a request counter.
    fn mock_server(script: Vec<(u16, &'static str)>) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&count);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0; len];
                reader.read_exact(&mut body).ok();
                let n = seen.fetch_add(1, Ordering::SeqCst);
                let (status, text) = script[n.min(script.len() - 1)];
                let payload = if status == 200 {
                    json!({
                        "answers": {},
                        "echo": serde_json::from_slice::<Value>(&body).unwrap(),
                        "raw": String::from_utf8_lossy(&body),
                    })
                    .to_string()
                } else {
                    text.to_owned()
                };
                let extra = if status == 429 {
                    "Retry-After: 0\r\n"
                } else {
                    ""
                };
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{payload}",
                    payload.len()
                )
                .ok();
            }
        });
        (url, count)
    }

    fn client(url: &str, cache: Option<&std::path::Path>) -> Client {
        Client::open(Config {
            api_key: "k".into(),
            cache_path: cache.map(|p| p.display().to_string()),
            concurrency: 4,
            base_url: url.into(),
            timeout_s: 10,
            max_retries: 2,
        })
        .unwrap()
    }

    fn ask(c: &Client, requests: &[Value]) -> Vec<Answer> {
        let texts: Vec<String> = requests.iter().map(Value::to_string).collect();
        c.ask_batch(&texts.iter().map(String::as_str).collect::<Vec<_>>())
    }

    #[test]
    fn caches_by_canonical_request() {
        let (url, count) = mock_server(vec![(200, "")]);
        let dir = std::env::temp_dir().join(format!("typesafe-test-{}", std::process::id()));
        let cache = dir.join("cache.db");
        let _ = std::fs::remove_file(&cache);
        let c = client(&url, Some(&cache));
        // The duplicate in the batch is sent once and answered from that.
        let first = ask(
            &c,
            &[json!({"b": 1, "a": 2}), json!({"x": 1}), json!({"x": 1})],
        );
        assert!(first.iter().all(|a| matches!(a, Ok((_, false)))));
        assert_eq!(first[2].as_ref().unwrap().0["echo"], json!({"x": 1}));
        assert_eq!(count.load(Ordering::SeqCst), 2);
        // Same request with keys in another order: a cache hit, in order.
        let again = ask(&c, &[json!({"x": 1}), json!({"a": 2, "b": 1})]);
        let (v, cached) = again[1].as_ref().unwrap();
        assert!(cached);
        assert_eq!(v["echo"], json!({"a": 2, "b": 1}));
        assert_eq!(count.load(Ordering::SeqCst), 2);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn sends_keys_in_written_order_without_whitespace() {
        let (url, _) = mock_server(vec![(200, "")]);
        let c = client(&url, None);
        let answers = c.ask_batch(&["{ \"z\": 1,\n  \"a\": \"x y\" }"]);
        let (v, _) = answers[0].as_ref().unwrap();
        assert_eq!(v["raw"], "{\"z\":1,\"a\":\"x y\"}");
    }

    #[test]
    fn retries_429_then_succeeds() {
        let (url, count) = mock_server(vec![(429, "slow down"), (200, "")]);
        let answers = ask(&client(&url, None), &[json!({"q": 1})]);
        assert!(matches!(answers[0], Ok((_, false))));
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn client_errors_are_per_request_and_not_retried() {
        let (url, count) = mock_server(vec![(400, "bad question")]);
        let answers = ask(&client(&url, None), &[json!({"q": 1})]);
        let err = answers[0].as_ref().unwrap_err();
        assert!(err.contains("400") && err.contains("bad question"), "{err}");
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
