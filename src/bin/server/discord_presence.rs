//! The session's Discord Rich Presence: "Listening to" the current track,
//! on the profile of whoever runs Discord on the machine running the server.
//! The card shows the title (linking to the playing source), the artist,
//! the cover with the album on hover, a progress bar, and the source's logo
//! in the corner.
//!
//! Like `media_controls`, it follows the session (`PlaybackHub::subscribe`),
//! so it's right whichever device plays. It talks to the Discord client over
//! its local IPC socket as the application given by `--discord-client-id`
//! (or `$DISCORD_CLIENT_ID`): one made at
//! https://discord.com/developers/applications, whose name heads the card.
//! Without one, it stays off.
//!
//! Discord has no paused activity: a paused track keeps its card, with a
//! pause sign for the logo and no progress bar, while a player is open to
//! resume it and for `PAUSED_FOR` at most. Discord needn't be running: the
//! server reconnects when it starts.

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail};
use musiclib_rs::musicdb::MusicDb;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    time::Instant,
};

use crate::playback::PlaybackHub;

/// Discord takes about 5 updates per 20 s; slower than that, a burst of
/// skips ends on its last track instead of queueing behind the limit.
const MIN_INTERVAL: Duration = Duration::from_secs(4);
/// How long the session stays unchanged before it's shown: a new track's
/// source, artist and duration arrive just after it starts.
const SETTLE: Duration = Duration::from_millis(1_500);
const RETRY: Duration = Duration::from_secs(20);
/// After Discord turns the application down (e.g. an unknown client id).
const REJECTED_RETRY: Duration = Duration::from_secs(5 * 60);
/// A client slower than this to answer is treated as gone.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// Start times closer than this are the same progress bar: snapshots
/// reach this task a little after they're taken.
const DRIFT_MS: i64 = 2_000;
/// How long a paused track stays on the card.
const PAUSED_FOR: Duration = Duration::from_secs(15 * 60);
/// The corner image while paused: Twemoji's ⏸ (CC-BY 4.0), at a pinned
/// version so the URL never changes under Discord's image cache.
const PAUSED_ICON: &str =
    "https://cdn.jsdelivr.net/gh/jdecked/twemoji@15.1.0/assets/72x72/23f8.png";
/// Discord's limit for an image or link URL in an activity.
const MAX_URL: usize = 256;

/// What the presence shows for the current track.
#[derive(Clone, Debug, Default, PartialEq)]
struct Presence {
    /// The current track's entry, whose cover and album (`Extras`) it shows.
    entry_id: Option<i64>,
    title: String,
    artist: Option<String>,
    /// The mounted source's provider, and its page when that's a public link.
    provider: Option<String>,
    url: Option<String>,
    /// Unix ms the track would have started at, given its position; `None`
    /// while paused.
    started_ms: Option<i64>,
    duration_ms: Option<u64>,
}

/// What the library adds for an entry: its cover and an album listing it.
#[derive(Clone, Debug, Default)]
struct Extras {
    cover: Option<String>,
    album: Option<String>,
}

impl Presence {
    /// The presence for a snapshot taken at `now_ms`; `None` with no track,
    /// or a paused one with no player open to resume it.
    fn from_snapshot(snapshot: &Value, now_ms: i64) -> Option<Self> {
        let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        let playing = snapshot["playing"].as_bool().unwrap_or(false);
        if !playing && snapshot["devices"].as_array().is_none_or(Vec::is_empty) {
            return None;
        }
        let current = &snapshot["current"];
        let mounted = &snapshot["mounted"];
        let position = snapshot["position_ms"].as_u64().unwrap_or(0);
        Some(Self {
            entry_id: current["entry_id"].as_i64(),
            title: text(&current["title"])?,
            // The library's credits, else what the playing source says.
            artist: text(&snapshot["artist"]).or_else(|| text(&mounted["artist"])),
            provider: text(&mounted["provider"]),
            url: text(&mounted["identifier"])
                .filter(|u| u.starts_with("https://") && u.len() <= MAX_URL),
            started_ms: playing.then(|| now_ms.saturating_sub_unsigned(position)),
            duration_ms: snapshot["duration_ms"].as_u64().filter(|&d| d > 0),
        })
    }

    /// Whether Discord already shows `other`, up to snapshot timing.
    fn same_as(&self, other: &Self) -> bool {
        let unpinned = |p: &Self| Self {
            started_ms: None,
            ..p.clone()
        };
        let same_progress = match (self.started_ms, other.started_ms) {
            (Some(a), Some(b)) => (a - b).abs() < DRIFT_MS,
            (a, b) => a.is_none() && b.is_none(),
        };
        same_progress && unpinned(self) == unpinned(other)
    }

    /// The `SET_ACTIVITY` payload.
    fn activity(&self, extras: &Extras) -> Value {
        let mut activity = json!({
            "type": 2, // Listening
            // The member list reads "Listening to <details>", not the app.
            "status_display_type": 2,
            "details": field(&self.title),
        });
        if let Some(url) = &self.url {
            activity["details_url"] = json!(url);
        }
        if let Some(start) = self.started_ms {
            activity["timestamps"] = json!({ "start": start });
            if let Some(d) = self.duration_ms {
                activity["timestamps"]["end"] = json!(start.saturating_add_unsigned(d));
            }
        }
        if let Some(artist) = &self.artist {
            activity["state"] = json!(field(artist));
        }
        let mut assets = json!({});
        // Discord proxies an external image given as the asset key.
        if let Some(cover) = &extras.cover
            && cover.starts_with("https://")
            && cover.len() <= MAX_URL
        {
            assets["large_image"] = json!(cover);
            if let Some(album) = &extras.album {
                assets["large_text"] = json!(field(album));
            }
            if let Some(url) = &self.url {
                assets["large_url"] = json!(url);
            }
        }
        let corner = match self.started_ms {
            None => Some((PAUSED_ICON.to_owned(), "Paused")),
            Some(_) => self
                .provider
                .as_deref()
                .and_then(|p| Some((provider_icon(p)?, provider_name(p)))),
        };
        if let Some((icon, text)) = corner {
            assets["small_image"] = json!(icon);
            assets["small_text"] = json!(field(text));
        }
        if assets.as_object().is_some_and(|a| !a.is_empty()) {
            activity["assets"] = assets;
        }
        activity
    }
}

/// A text field as Discord takes it: 2 to 128 characters.
fn field(s: &str) -> String {
    let mut out: String = s.chars().take(128).collect();
    if out.chars().count() == 128 && s.chars().nth(128).is_some() {
        out.pop();
        out.push('…');
    }
    if out.chars().count() < 2 {
        out.push('\u{200b}');
    }
    out
}

fn provider_name(provider: &str) -> &str {
    match provider {
        "youtube" => "YouTube",
        "spotify" => "Spotify",
        "soundcloud" => "SoundCloud",
        "nicovideo" => "Niconico",
        "bandcamp" => "Bandcamp",
        other => other,
    }
}

/// The provider's logo: its site's icon from Google's favicon service, which
/// serves a PNG at a fixed URL (Discord can't show an `.ico` or SVG).
fn provider_icon(provider: &str) -> Option<String> {
    let site = match provider {
        "youtube" => "https://www.youtube.com",
        "spotify" => "https://open.spotify.com",
        "soundcloud" => "https://soundcloud.com",
        "nicovideo" => "https://www.nicovideo.jp",
        "bandcamp" => "https://bandcamp.com",
        _ => return None,
    };
    Some(format!(
        "https://t3.gstatic.com/faviconV2?client=SOCIAL&type=FAVICON\
         &fallback_opts=TYPE,SIZE,URL&url={site}&size=128"
    ))
}

/// The library's cover and album for `entry_id`; a failed lookup is left out.
async fn extras(db: &MusicDb, entry_id: i64) -> Extras {
    let cover = crate::cover::resolve(db, entry_id)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!("cover for entry {entry_id}: {e}");
            None
        });
    let album = album(db, entry_id).await.unwrap_or_else(|e| {
        tracing::warn!("album for entry {entry_id}: {e}");
        None
    });
    Extras { cover, album }
}

/// The title of a release listing the track `entry_id`: an album before a
/// single (often named after the track), then by the cover's provider
/// preference. A playlist isn't an album.
async fn album(db: &MusicDb, entry_id: i64) -> anyhow::Result<Option<String>> {
    let members = crate::entries::component_members(db, entry_id).await?;
    let pairs = db.pairs_by_entry_ids(&members).await?;
    let parents: Vec<(String, String)> = db
        .child_rows_for_child_pairs(&pairs)
        .await?
        .into_iter()
        .map(|c| (c.parent_source, c.parent_identifier))
        .collect();
    if parents.is_empty() {
        return Ok(None);
    }
    let sources = db.source_rows_for_pairs(&parents).await?;
    let ids: Vec<i64> = sources.iter().map(|s| s.entry_id).collect();
    let releases: HashSet<i64> = db
        .entry_rows_by_ids(&ids)
        .await?
        .into_iter()
        .filter(|e| e.entry_type == "release")
        .map(|e| e.id)
        .collect();
    let mut albums: Vec<_> = sources
        .into_iter()
        .filter(|s| releases.contains(&s.entry_id))
        .filter(|s| s.release_type.as_deref() != Some("playlist"))
        .map(|s| {
            let single = s.release_type.as_deref() == Some("single");
            let rank = crate::cover::provider_rank(&s.source);
            ((single, rank), (s.source, s.identifier))
        })
        .collect();
    albums.sort();
    let albums: Vec<(String, String)> = albums.into_iter().map(|(_, pair)| pair).collect();
    let titles = db.pair_titles(&albums).await?;
    Ok(albums.iter().find_map(|pair| titles.get(pair).cloned()))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
const OP_CLOSE: u32 = 2;
const OP_PING: u32 = 3;
const OP_PONG: u32 = 4;

/// Where Discord listens: `discord-ipc-0` to `-9` in the runtime or temp
/// directory, or in a Flatpak's or Snap's subdirectory of it.
fn socket_candidates() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .collect();
    dirs.push("/tmp".into());
    let mut paths = Vec::new();
    for dir in dirs {
        for sub in [
            "",
            "app/com.discordapp.Discord",
            "snap.discord",
            ".flatpak/dev.vencord.Vesktop/xdg-run",
        ] {
            for i in 0..10 {
                let path = dir.join(sub).join(format!("discord-ipc-{i}"));
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
    }
    paths
}

/// Why a connection attempt failed.
#[derive(Debug)]
enum ConnectError {
    /// No client is listening.
    Absent,
    /// A client answered and turned the application down.
    Rejected(anyhow::Error),
}

/// A connection to the Discord client, past its handshake.
struct Ipc {
    stream: UnixStream,
    nonce: u64,
    /// Received bytes not yet taken as a whole frame.
    buf: Vec<u8>,
}

impl Ipc {
    async fn connect(client_id: &str) -> Result<Self, ConnectError> {
        for path in socket_candidates() {
            if let Ok(stream) = UnixStream::connect(&path).await {
                return Self::handshake(stream, client_id)
                    .await
                    .map_err(ConnectError::Rejected);
            }
        }
        Err(ConnectError::Absent)
    }

    #[cfg(test)]
    async fn connect_at(path: &std::path::Path, client_id: &str) -> anyhow::Result<Self> {
        Self::handshake(UnixStream::connect(path).await?, client_id).await
    }

    async fn handshake(stream: UnixStream, client_id: &str) -> anyhow::Result<Self> {
        let mut ipc = Self {
            stream,
            nonce: 0,
            buf: Vec::new(),
        };
        ipc.write(OP_HANDSHAKE, &json!({ "v": 1, "client_id": client_id }))
            .await?;
        let ready = ipc.read().await?;
        if ready["evt"] != "READY" {
            bail!("unexpected handshake reply {ready}");
        }
        Ok(ipc)
    }

    async fn write(&mut self, op: u32, payload: &Value) -> anyhow::Result<()> {
        let body = payload.to_string();
        let mut frame = Vec::with_capacity(8 + body.len());
        frame.extend_from_slice(&op.to_le_bytes());
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(body.as_bytes());
        self.stream.write_all(&frame).await?;
        Ok(())
    }

    /// The next whole frame in `buf`, if there is one.
    fn next_frame(&mut self) -> anyhow::Result<Option<(u32, Value)>> {
        let Some(header) = self.buf.get(..8) else {
            return Ok(None);
        };
        let op = u32::from_le_bytes(header[..4].try_into()?);
        let len = u32::from_le_bytes(header[4..].try_into()?) as usize;
        let Some(body) = self.buf.get(8..8 + len) else {
            return Ok(None);
        };
        let payload = serde_json::from_slice(body).context("Discord sent invalid JSON")?;
        self.buf.drain(..8 + len);
        Ok(Some((op, payload)))
    }

    /// Answers a ping or a close; a reply or event goes back to the caller.
    async fn dispatch(&mut self, op: u32, payload: Value) -> anyhow::Result<Option<Value>> {
        match op {
            OP_FRAME => Ok(Some(payload)),
            OP_PING => self.write(OP_PONG, &payload).await.map(|()| None),
            OP_CLOSE => bail!("Discord closed the connection: {}", payload["message"]),
            _ => Ok(None),
        }
    }

    /// The next reply or event, answering pings on the way.
    async fn read(&mut self) -> anyhow::Result<Value> {
        tokio::time::timeout(REPLY_TIMEOUT, async {
            loop {
                while let Some((op, payload)) = self.next_frame()? {
                    if let Some(reply) = self.dispatch(op, payload).await? {
                        return Ok(reply);
                    }
                }
                let mut chunk = [0; 4096];
                let n = self.stream.read(&mut chunk).await?;
                if n == 0 {
                    bail!("Discord closed the connection");
                }
                self.buf.extend_from_slice(&chunk[..n]);
            }
        })
        .await
        .context("Discord didn't answer")?
    }

    /// Shows `activity`, or clears it with `None`.
    async fn set_activity(&mut self, activity: Option<Value>) -> anyhow::Result<()> {
        self.nonce += 1;
        let nonce = self.nonce.to_string();
        let request = json!({
            "cmd": "SET_ACTIVITY",
            "args": { "pid": std::process::id(), "activity": activity },
            "nonce": nonce,
        });
        self.write(OP_FRAME, &request).await?;
        loop {
            let reply = self.read().await?;
            if reply["nonce"] != nonce.as_str() {
                continue;
            }
            // A refused activity leaves the connection usable.
            if reply["evt"] == "ERROR" {
                tracing::warn!("Discord refused the presence: {}", reply["data"]["message"]);
            }
            return Ok(());
        }
    }

    /// Handles whatever arrived unasked, without waiting: readiness can
    /// be stale. An error means Discord is gone.
    async fn drain(&mut self) -> anyhow::Result<()> {
        let mut chunk = [0; 4096];
        loop {
            match self.stream.try_read(&mut chunk) {
                Ok(0) => bail!("Discord closed the connection"),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        while let Some((op, payload)) = self.next_frame()? {
            self.dispatch(op, payload).await?;
        }
        Ok(())
    }
}

/// Runs the presence for the life of the server.
pub async fn run(hub: Arc<PlaybackHub>, db: MusicDb, client_id: String) {
    tracing::info!("Discord presence on, as application {client_id}");
    let mut snapshots = hub.subscribe();
    // When the latest snapshot arrived: its position is as of then.
    let mut received = (Instant::now(), now_ms());
    let mut ipc: Option<Ipc> = None;
    let mut retry_at = Instant::now();
    // What Discord shows; a new connection starts with nothing.
    let mut shown: Option<Presence> = None;
    let mut sent_at: Option<Instant> = None;
    let mut paused_since: Option<Instant> = None;
    let mut looked_up: Option<(i64, Extras)> = None;
    loop {
        let now = Instant::now();
        let mut want = serde_json::from_str::<Value>(&snapshots.borrow())
            .ok()
            .and_then(|v| Presence::from_snapshot(&v, received.1));
        let mut wake = None;
        match &want {
            Some(p) if p.started_ms.is_none() => {
                let until = *paused_since.get_or_insert(now) + PAUSED_FOR;
                if now >= until {
                    want = None;
                } else {
                    wake = Some(until);
                }
            }
            _ => paused_since = None,
        }
        if ipc.is_none() && now >= retry_at {
            match Ipc::connect(&client_id).await {
                Ok(c) => {
                    tracing::info!("Discord presence connected");
                    ipc = Some(c);
                    shown = None;
                }
                Err(ConnectError::Absent) => retry_at = now + RETRY,
                Err(ConnectError::Rejected(e)) => {
                    tracing::warn!("Discord presence: {e:#}");
                    retry_at = now + REJECTED_RETRY;
                }
            }
        }
        let unchanged = match (&shown, &want) {
            (Some(a), Some(b)) => a.same_as(b),
            (a, b) => a.is_none() && b.is_none(),
        };
        if let Some(conn) = ipc.as_mut()
            && !unchanged
        {
            let ready = (received.0 + SETTLE).max(sent_at.map_or(now, |t| t + MIN_INTERVAL));
            if ready > now {
                wake = Some(wake.map_or(ready, |w: Instant| w.min(ready)));
            } else {
                let activity = match &want {
                    Some(p) => {
                        let extras = match p.entry_id {
                            Some(id) => {
                                if looked_up.as_ref().is_none_or(|(at, _)| *at != id) {
                                    looked_up = Some((id, extras(&db, id).await));
                                }
                                looked_up
                                    .as_ref()
                                    .map(|(_, e)| e.clone())
                                    .unwrap_or_default()
                            }
                            None => Extras::default(),
                        };
                        Some(p.activity(&extras))
                    }
                    None => None,
                };
                match conn.set_activity(activity).await {
                    Ok(()) => {
                        shown = want;
                        sent_at = Some(Instant::now());
                    }
                    Err(e) => {
                        tracing::info!("Discord presence disconnected: {e:#}");
                        ipc = None;
                        retry_at = Instant::now() + RETRY;
                    }
                }
                // Look again: the session may have moved on meanwhile.
                continue;
            }
        }
        if ipc.is_none() {
            wake = Some(wake.map_or(retry_at, |w: Instant| w.min(retry_at)));
        }
        tokio::select! {
            changed = snapshots.changed() => {
                if changed.is_err() {
                    return;
                }
                received = (Instant::now(), now_ms());
            }
            _ = tokio::time::sleep_until(wake.unwrap_or(now)), if wake.is_some() => {}
            readable = async { ipc.as_ref().expect("guarded").stream.readable().await },
                if ipc.is_some() =>
            {
                let gone = match readable {
                    Ok(()) => ipc.as_mut().expect("guarded").drain().await.err(),
                    Err(e) => Some(e.into()),
                };
                if let Some(e) = gone {
                    tracing::info!("Discord presence disconnected: {e:#}");
                    ipc = None;
                    retry_at = Instant::now() + RETRY;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::net::UnixListener;

    use super::*;

    fn playing(position_ms: u64) -> Value {
        json!({
            "devices": [{ "id": "d", "name": "Firefox" }],
            "current": { "entry_id": 7, "title": "LETTER☆彡" },
            "artist": "Shirakami Fubuki",
            "mounted": {
                "provider": "youtube",
                "identifier": "https://youtu.be/abc",
                "artist": "Fubuki Ch.",
            },
            "playing": true,
            "position_ms": position_ms,
            "duration_ms": 200_000,
        })
    }

    fn paused() -> Value {
        let mut s = playing(30_000);
        s["playing"] = json!(false);
        s
    }

    const COVER: &str = "https://i.ytimg.com/vi/abc/mqdefault.jpg";

    #[test]
    fn a_playing_snapshot_becomes_a_listening_activity() {
        let p = Presence::from_snapshot(&playing(1_500), 10_000).unwrap();
        assert_eq!(p.started_ms, Some(8_500));
        let extras = Extras {
            cover: Some(COVER.into()),
            album: Some("Fubuki's Album".into()),
        };
        assert_eq!(
            p.activity(&extras),
            json!({
                "type": 2,
                "status_display_type": 2,
                "details": "LETTER☆彡",
                "details_url": "https://youtu.be/abc",
                "state": "Shirakami Fubuki",
                "timestamps": { "start": 8_500, "end": 208_500 },
                "assets": {
                    "large_image": COVER,
                    "large_text": "Fubuki's Album",
                    "large_url": "https://youtu.be/abc",
                    "small_image": provider_icon("youtube").unwrap(),
                    "small_text": "YouTube",
                },
            })
        );
    }

    #[test]
    fn a_paused_track_shows_a_pause_sign_and_no_progress() {
        let p = Presence::from_snapshot(&paused(), 10_000).unwrap();
        assert_eq!(p.started_ms, None);
        let activity = p.activity(&Extras::default());
        assert_eq!(activity.get("timestamps"), None);
        assert_eq!(
            activity["assets"],
            json!({ "small_image": PAUSED_ICON, "small_text": "Paused" })
        );
        // Resuming is a change; a paused card at a later time isn't.
        assert!(p.same_as(&Presence::from_snapshot(&paused(), 99_000).unwrap()));
        assert!(!p.same_as(&Presence::from_snapshot(&playing(30_000), 10_000).unwrap()));
    }

    #[test]
    fn nothing_to_resume_shows_nothing() {
        let mut closed = paused();
        closed["devices"] = json!([]);
        assert_eq!(Presence::from_snapshot(&closed, 0), None);
        assert_eq!(
            Presence::from_snapshot(&json!({ "current": null, "playing": true }), 0),
            None
        );
    }

    #[test]
    fn an_unmounted_track_shows_its_title_alone() {
        let p = Presence::from_snapshot(
            &json!({
                "current": { "entry_id": 7, "title": "x" },
                "mounted": null,
                "playing": true,
                "position_ms": 0,
                "duration_ms": null,
            }),
            5,
        )
        .unwrap();
        assert_eq!(
            p.activity(&Extras::default()),
            json!({
                "type": 2,
                "status_display_type": 2,
                "details": "x\u{200b}",
                "timestamps": { "start": 5 },
            })
        );
    }

    #[test]
    fn timing_jitter_is_the_same_presence_but_a_seek_is_not() {
        let a = Presence::from_snapshot(&playing(10_000), 100_000).unwrap();
        let jitter = Presence::from_snapshot(&playing(10_000), 101_000).unwrap();
        let seek = Presence::from_snapshot(&playing(60_000), 100_000).unwrap();
        assert!(a.same_as(&jitter));
        assert!(!a.same_as(&seek));
    }

    #[test]
    fn long_text_is_cut_to_discords_limit() {
        let long = "あ".repeat(200);
        let cut = field(&long);
        assert_eq!(cut.chars().count(), 128);
        assert!(cut.ends_with('…'));
        assert_eq!(field(&"a".repeat(128)), "a".repeat(128));
    }

    async fn read_frame(stream: &mut UnixStream) -> (u32, Value) {
        let mut header = [0; 8];
        stream.read_exact(&mut header).await.unwrap();
        let op = u32::from_le_bytes(header[..4].try_into().unwrap());
        let mut body = vec![0; u32::from_le_bytes(header[4..].try_into().unwrap()) as usize];
        stream.read_exact(&mut body).await.unwrap();
        (op, serde_json::from_slice(&body).unwrap())
    }

    async fn write_frame(stream: &mut UnixStream, op: u32, payload: Value) {
        let body = payload.to_string();
        stream.write_all(&op.to_le_bytes()).await.unwrap();
        stream
            .write_all(&(body.len() as u32).to_le_bytes())
            .await
            .unwrap();
        stream.write_all(body.as_bytes()).await.unwrap();
    }

    #[tokio::test]
    async fn speaks_discords_ipc_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("discord-ipc-0");
        let listener = UnixListener::bind(&path).unwrap();
        let discord = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let (op, hello) = read_frame(&mut s).await;
            assert_eq!(
                (op, hello),
                (OP_HANDSHAKE, json!({ "v": 1, "client_id": "42" }))
            );
            write_frame(
                &mut s,
                OP_FRAME,
                json!({ "cmd": "DISPATCH", "evt": "READY" }),
            )
            .await;

            let (op, set) = read_frame(&mut s).await;
            assert_eq!(op, OP_FRAME);
            assert_eq!(set["cmd"], "SET_ACTIVITY");
            assert_eq!(set["args"]["activity"]["details"], "hi");
            // A ping in between is answered, not taken for the reply.
            write_frame(&mut s, OP_PING, json!({ "n": 1 })).await;
            assert_eq!(read_frame(&mut s).await, (OP_PONG, json!({ "n": 1 })));
            let nonce = set["nonce"].clone();
            write_frame(
                &mut s,
                OP_FRAME,
                json!({ "cmd": "SET_ACTIVITY", "nonce": nonce }),
            )
            .await;

            let (_, clear) = read_frame(&mut s).await;
            assert_eq!(clear["args"]["activity"], Value::Null);
            let nonce = clear["nonce"].clone();
            write_frame(
                &mut s,
                OP_FRAME,
                json!({ "cmd": "SET_ACTIVITY", "nonce": nonce }),
            )
            .await;
        });
        let mut ipc = Ipc::connect_at(&path, "42").await.unwrap();
        // With nothing sent, a drain returns at once, even when the socket
        // still reads as ready from the handshake.
        let _ = tokio::time::timeout(Duration::from_millis(100), ipc.stream.readable()).await;
        tokio::time::timeout(Duration::from_secs(1), ipc.drain())
            .await
            .expect("drain waited")
            .unwrap();
        ipc.set_activity(Some(json!({ "details": "hi" })))
            .await
            .unwrap();
        ipc.set_activity(None).await.unwrap();
        discord.await.unwrap();
        // Discord quitting is noticed.
        assert!(ipc.drain().await.is_err());
    }
}
