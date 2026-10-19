//! The player's single playback session, Spotify-Connect style — see
//! docs/plan-single-user-player.md.
//!
//! One session per server: the queue, what's playing, position, and
//! volume all live here, not in a browser tab. Each tab is a *device*
//! subscribed to `GET /api/playback/events` (SSE); at most one is *active*
//! and actually mounts the embed, every other one is a remote control.
//! Everything a user does is a command (`POST /api/playback/command`); the
//! active device reports what really happened (`POST /api/playback/report`)
//! and every change is broadcast back as a full snapshot.

use std::{
    collections::BTreeMap,
    convert::Infallible,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use futures::StreamExt as _;
use http_body_util::{BodyExt as _, StreamBody};
use hyper::{Response, StatusCode, body::Frame};
use musiclib_rs::musicdb::{MainCredit, MusicDb};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::watch;

use crate::{
    respond::{self, HyperResponse},
    state::AppState,
};

/// Keep at least this many tracks ahead of the current one (when repeat is
/// off): below it the snapshot asks for a top-up (`wants_recommend`), which
/// the active device answers with a shuffle-mix batch (`Command::Recommend`).
/// The mix's settings and play log live in the browser, so the batch does
/// too.
const AUTO_RECOMMEND_LOOKAHEAD: usize = 3;
/// Played tracks kept behind the current one before the oldest are dropped.
const MAX_HISTORY: usize = 100;
/// Progress reports that land within this of where the session already
/// extrapolates the position aren't rebroadcast — remotes interpolate.
/// Tight enough for remotes' muted video mirrors to follow the audio (an
/// active device stalled on buffering is otherwise invisible to them).
const DRIFT_TOLERANCE_MS: u64 = 500;
/// SSE comment interval: also how quickly a silently vanished client (no
/// FIN) gets noticed, since that only surfaces as a failed write.
const KEEPALIVE: Duration = Duration::from_secs(15);
const PERSIST_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct QueueItem {
    pub entry_id: i64,
    #[serde(default)]
    pub title: Option<String>,
    /// The album a tracklist was played from (the now-playing drawer scores
    /// it alongside the track).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_id: Option<i64>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Repeat {
    #[default]
    Off,
    All,
    One,
}

/// Play order, shuffle and repeat — a port of what used to be the
/// frontend's `Queue` module, same semantics.
///
/// `items` is the queue in the order it was built; `order` is the
/// permutation of indices into `items` that playback walks (identity
/// unless shuffled), and `pos` indexes `order`. Three layers in play order:
/// the current item, the "Queue" layer (explicit one-at-a-time picks,
/// `order[pos+1 .. queue_end)`), then "Post-queue" (`order[queue_end ..]`:
/// a tracklist's continuation, a shuffle batch, auto-recommended filler).
/// `set` puts its whole batch in Post-queue, so a later `queue_next` still
/// lands right after the current track.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Queue {
    items: Vec<QueueItem>,
    order: Vec<usize>,
    pos: usize,
    queue_end: usize,
    shuffled: bool,
    repeat: Repeat,
}

impl Queue {
    fn current(&self) -> Option<&QueueItem> {
        self.order.get(self.pos).map(|&i| &self.items[i])
    }

    fn upcoming_len(&self) -> usize {
        self.order.len().saturating_sub(self.pos + 1)
    }

    /// Everything except `anchor`, shuffled, with `anchor` pinned first.
    fn shuffle_from(&self, anchor: usize) -> Vec<usize> {
        let mut rest: Vec<usize> = (0..self.items.len()).filter(|&i| i != anchor).collect();
        fastrand::shuffle(&mut rest);
        std::iter::once(anchor).chain(rest).collect()
    }

    fn set(&mut self, items: Vec<QueueItem>, start_index: usize) {
        self.items = items;
        if self.items.is_empty() {
            self.order.clear();
            self.pos = 0;
            self.queue_end = 0;
            return;
        }
        let anchor = if start_index < self.items.len() {
            start_index
        } else {
            0
        };
        self.order = if self.shuffled {
            self.shuffle_from(anchor)
        } else {
            (0..self.items.len()).collect()
        };
        self.pos = self.order.iter().position(|&i| i == anchor).unwrap_or(0);
        self.queue_end = self.pos + 1;
    }

    fn has_prev(&self) -> bool {
        !self.order.is_empty() && (self.pos > 0 || self.repeat == Repeat::All)
    }

    fn has_next(&self) -> bool {
        !self.order.is_empty() && (self.pos + 1 < self.order.len() || self.repeat == Repeat::All)
    }

    fn prev(&mut self) -> bool {
        if self.order.is_empty() {
            return false;
        }
        if self.pos > 0 {
            self.pos -= 1;
        } else if self.repeat == Repeat::All {
            self.pos = self.order.len() - 1;
        } else {
            return false;
        }
        true
    }

    fn next(&mut self) -> bool {
        if self.order.is_empty() {
            return false;
        }
        if self.pos + 1 < self.order.len() {
            self.pos += 1;
        } else if self.repeat == Repeat::All {
            self.pos = 0;
        } else {
            return false;
        }
        self.trim_history();
        true
    }

    fn jump_to(&mut self, order_index: usize) -> bool {
        if order_index >= self.order.len() {
            return false;
        }
        self.pos = order_index;
        self.trim_history();
        true
    }

    fn remove_at(&mut self, order_index: usize) -> bool {
        if order_index == self.pos || order_index >= self.order.len() {
            return false;
        }
        self.order.remove(order_index);
        if order_index < self.pos {
            self.pos -= 1;
        }
        if order_index < self.queue_end {
            self.queue_end -= 1;
        }
        self.compact();
        true
    }

    fn toggle_shuffle(&mut self) {
        self.shuffled = !self.shuffled;
        let Some(&anchor) = self.order.get(self.pos) else {
            return;
        };
        self.order = if self.shuffled {
            self.shuffle_from(anchor)
        } else {
            (0..self.items.len()).collect()
        };
        self.pos = self.order.iter().position(|&i| i == anchor).unwrap_or(0);
        // Reshuffling scrambles the Queue/Post-queue split too; treat the
        // reshuffled upcoming list as pure Post-queue.
        self.queue_end = self.pos + 1;
    }

    fn cycle_repeat(&mut self) {
        self.repeat = match self.repeat {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        };
    }

    /// Into the Queue layer: after whatever's already queued, ahead of
    /// Post-queue (and never behind a stale `queue_end` playback has since
    /// passed).
    fn queue_next(&mut self, item: QueueItem) {
        let idx = self.items.len();
        self.items.push(item);
        let insert_at = self.queue_end.max(self.pos + 1).min(self.order.len());
        self.order.insert(insert_at, idx);
        self.queue_end = insert_at + 1;
    }

    /// Swaps the current item (the dock's edition switcher) without moving
    /// `pos` or touching the rest of the play order.
    fn replace_current(&mut self, item: QueueItem) -> bool {
        let Some(&idx) = self.order.get(self.pos) else {
            return false;
        };
        self.items[idx] = item;
        true
    }

    fn append_post_queue(&mut self, items: Vec<QueueItem>) {
        let start = self.items.len();
        self.order.extend(start..start + items.len());
        self.items.extend(items);
    }

    /// Bounds a long-running session: forget the oldest played tracks.
    fn trim_history(&mut self) {
        if self.pos <= MAX_HISTORY {
            return;
        }
        let drop = self.pos - MAX_HISTORY;
        self.order.drain(..drop);
        self.pos -= drop;
        self.queue_end = self.queue_end.saturating_sub(drop);
        self.compact();
    }

    /// Drops `items` no longer referenced by `order` (removed or trimmed).
    fn compact(&mut self) {
        if self.items.len() == self.order.len() {
            return;
        }
        let mut remap = vec![usize::MAX; self.items.len()];
        let mut items = Vec::with_capacity(self.order.len());
        for &i in &self.order {
            if remap[i] == usize::MAX {
                remap[i] = items.len();
                items.push(self.items[i].clone());
            }
        }
        for i in &mut self.order {
            *i = remap[*i];
        }
        self.items = items;
    }

    fn rows(&self) -> Vec<serde_json::Value> {
        self.order
            .iter()
            .enumerate()
            .map(|(i, &item)| {
                let it = &self.items[item];
                json!({
                    "entry_id": it.entry_id,
                    "title": it.title,
                    "order_index": i,
                    "is_current": i == self.pos,
                    "is_post_queue": i >= self.queue_end,
                })
            })
            .collect()
    }
}

/// Two dials whose product is the effective volume (0..100 each): `global`
/// scales everything, `by_provider` is a per-provider balance.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Volume {
    global: u8,
    #[serde(default)]
    by_provider: BTreeMap<String, u8>,
    #[serde(default)]
    muted: bool,
}

/// What the active device actually mounted for the current track.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Mounted {
    provider: String,
    identifier: String,
    /// What the source says about the artist (a YouTube channel, Spotify's
    /// artists): the queue item only has a title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    artist: Option<String>,
    /// False for a bare embed the device can't drive (no play/pause/volume),
    /// e.g. a Spotify preview iframe on a device not connected to Spotify.
    #[serde(default = "default_true")]
    controllable: bool,
}

fn default_true() -> bool {
    true
}

struct Device {
    name: String,
    connections: usize,
}

/// What survives a restart (always paused).
#[derive(Default, Serialize, Deserialize)]
struct Persisted {
    queue: Queue,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    position_ms: u64,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    volume: Option<Volume>,
    #[serde(default)]
    last_active_device: Option<String>,
}

#[derive(Default)]
struct Session {
    queue: Queue,
    /// Bumped every time the current item (re)starts, including a
    /// repeat-one replay or a jump to the same row: the active device
    /// remounts when it changes, and stale reports are told apart by it.
    track_seq: u64,
    /// Source explicitly picked with the switcher; `None` lets the active
    /// device pick by its own provider preference.
    source: Option<String>,
    mounted: Option<Mounted>,
    /// Desired state from commands, overwritten by the active device's
    /// reports of what actually happened.
    playing: bool,
    /// Position as of `position_at` (advancing from there while playing).
    position_ms: u64,
    position_at: Option<Instant>,
    duration_ms: Option<u64>,
    seek_seq: u64,
    /// `None` until some device seeds it from its old local settings.
    volume: Option<Volume>,
    active_device: Option<String>,
    last_active_device: Option<String>,
    devices: BTreeMap<String, Device>,
    version: u64,
    /// Set when a track ended with nothing after it: once a recommendation
    /// lands, advance if this is still the track.
    advance_after_recommend: Option<u64>,
    /// The library's artist line for a track (`artist_loop`).
    credited: Option<Credited>,
}

/// `artists`, as credited for `entry_id` with `source` mounted.
struct Credited {
    entry_id: i64,
    source: Option<String>,
    artists: Vec<CreditedArtist>,
    /// Set by `recredit`: look the names up again.
    stale: bool,
}

/// One name on the artist line, with the artist entry it opens.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct CreditedArtist {
    pub(crate) entry_id: i64,
    pub(crate) name: String,
}

impl Session {
    fn position_now(&self) -> u64 {
        let mut p = self.position_ms;
        if self.playing
            && let Some(at) = self.position_at
        {
            p += at.elapsed().as_millis() as u64;
        }
        match self.duration_ms {
            Some(d) if d > 0 => p.min(d),
            _ => p,
        }
    }

    fn freeze(&mut self) {
        self.position_ms = self.position_now();
        self.position_at = Some(Instant::now());
    }

    fn set_playing(&mut self, playing: bool) {
        self.freeze();
        self.playing = playing;
    }

    /// The current queue item starts over from the top (a new track, or a
    /// replay of the same one).
    fn start_track(&mut self, keep_source: bool) {
        self.track_seq += 1;
        if !keep_source {
            self.source = None;
        }
        self.mounted = None;
        self.position_ms = 0;
        self.position_at = Some(Instant::now());
        self.duration_ms = None;
        self.playing = self.queue.current().is_some();
    }

    /// Spotify's "nothing is playing anywhere, so play here": a command
    /// that starts playback with no active device makes its sender active.
    fn claim_if_idle(&mut self, device: &str) {
        if self.active_device.is_none() && self.devices.contains_key(device) {
            self.active_device = Some(device.to_owned());
            self.last_active_device = Some(device.to_owned());
        }
    }

    fn needs_recommend(&self) -> bool {
        self.queue.repeat == Repeat::Off
            && self.queue.current().is_some()
            && self.queue.upcoming_len() < AUTO_RECOMMEND_LOOKAHEAD
    }

    fn snapshot(&self) -> serde_json::Value {
        let devices: Vec<_> = self
            .devices
            .iter()
            .map(|(id, d)| json!({ "id": id, "name": d.name }))
            .collect();
        json!({
            "version": self.version,
            "devices": devices,
            "active_device": self.active_device,
            "current": self.queue.current(),
            "artist": self.artist(),
            "artists": self.artists(),
            "track_seq": self.track_seq,
            "source": self.source,
            "mounted": self.mounted,
            "playing": self.playing,
            "position_ms": self.position_now(),
            "duration_ms": self.duration_ms,
            "seek_seq": self.seek_seq,
            "shuffled": self.queue.shuffled,
            "repeat": self.queue.repeat,
            "has_prev": self.queue.has_prev(),
            "has_next": self.queue.has_next(),
            "queue": self.queue.rows(),
            "wants_recommend": self.needs_recommend(),
            "volume": self.volume,
        })
    }

    /// The current track's credited artists from the library, kept from
    /// the last mounted source until the new one's are looked up.
    fn artists(&self) -> &[CreditedArtist] {
        match (&self.credited, self.queue.current()) {
            (Some(c), Some(current)) if c.entry_id == current.entry_id => &c.artists,
            _ => &[],
        }
    }

    /// `artists` as one line, for the OS media controls.
    fn artist(&self) -> Option<String> {
        let names: Vec<&str> = self.artists().iter().map(|a| a.name.as_str()).collect();
        (!names.is_empty()).then(|| names.join(", "))
    }

    /// What `artist_loop` should look up next, if the current track or its
    /// mounted source changed since the last lookup.
    fn uncredited(&self) -> Option<(i64, Option<String>)> {
        let entry_id = self.queue.current()?.entry_id;
        let source = self.mounted.as_ref().map(|m| m.identifier.clone());
        match &self.credited {
            Some(c)
                if !c.stale
                    && c.entry_id == entry_id
                    && (c.source == source || source.is_none()) =>
            {
                None
            }
            _ => Some((entry_id, source)),
        }
    }

    fn persisted(&self) -> Persisted {
        Persisted {
            queue: self.queue.clone(),
            source: self.source.clone(),
            position_ms: self.position_now(),
            duration_ms: self.duration_ms,
            volume: self.volume.clone(),
            last_active_device: self.last_active_device.clone(),
        }
    }
}

pub struct PlaybackHub {
    session: Mutex<Session>,
    tx: watch::Sender<Arc<str>>,
    dirty: AtomicBool,
    path: PathBuf,
}

impl PlaybackHub {
    /// Restores the session saved at `path` (paused), or starts empty.
    pub fn load(path: PathBuf) -> Arc<Self> {
        let persisted: Persisted = std::fs::read(&path)
            .ok()
            .and_then(|b| match serde_json::from_slice(&b) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!("ignoring unreadable {}: {e}", path.display());
                    None
                }
            })
            .unwrap_or_default();
        let session = Session {
            queue: persisted.queue,
            source: persisted.source,
            position_ms: persisted.position_ms,
            position_at: Some(Instant::now()),
            duration_ms: persisted.duration_ms,
            volume: persisted.volume,
            last_active_device: persisted.last_active_device,
            ..Default::default()
        };
        let (tx, _) = watch::channel(Arc::from(session.snapshot().to_string()));
        Arc::new(Self {
            session: Mutex::new(session),
            tx,
            dirty: AtomicBool::new(false),
            path,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Session> {
        self.session.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Every snapshot from now on, starting with the current one.
    pub fn subscribe(&self) -> watch::Receiver<Arc<str>> {
        self.tx.subscribe()
    }

    /// A command from the OS media controls (`media_controls`). They aren't
    /// a device and can't play anything, so they act as the device that
    /// last played: play resumes there while it's connected. With no such
    /// device, only pausing and seeking apply.
    #[cfg(feature = "media_controls")]
    pub fn media_key(&self, key: MediaKey) {
        let mut s = self.lock();
        let device = s
            .active_device
            .clone()
            .or_else(|| s.last_active_device.clone())
            .filter(|d| s.devices.contains_key(d));
        let command = match key {
            MediaKey::Pause | MediaKey::Stop => Command::Pause,
            MediaKey::SeekTo(ms) => Command::Seek { position_ms: ms },
            MediaKey::SeekBy(delta) => {
                let target = s.position_now().saturating_add_signed(delta);
                Command::Seek {
                    position_ms: s.duration_ms.map_or(target, |d| target.min(d)),
                }
            }
            _ if device.is_none() => return,
            MediaKey::Play => Command::Play,
            MediaKey::Toggle => Command::Toggle,
            MediaKey::Next => Command::Next,
            MediaKey::Prev => Command::Prev,
        };
        if apply(&mut s, device.as_deref().unwrap_or(""), command).is_ok() {
            self.publish(&mut s);
        }
    }

    fn publish(&self, s: &mut Session) {
        s.version += 1;
        self.tx.send_replace(Arc::from(s.snapshot().to_string()));
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Writes the session to disk at most every `PERSIST_INTERVAL`, only
    /// after something changed. Runs for the life of the server.
    pub async fn persist_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(PERSIST_INTERVAL);
        loop {
            tick.tick().await;
            if !self.dirty.swap(false, Ordering::Relaxed) {
                continue;
            }
            let bytes = match serde_json::to_vec(&self.lock().persisted()) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("serializing playback session: {e}");
                    continue;
                }
            };
            let tmp = self.path.with_extension("json.tmp");
            let res = async {
                tokio::fs::write(&tmp, &bytes).await?;
                tokio::fs::rename(&tmp, &self.path).await
            }
            .await;
            if let Err(e) = res {
                tracing::warn!("saving {}: {e}", self.path.display());
            }
        }
    }

    /// Fills in the current track's artist line from the library's credits
    /// (`artist_line`) whenever the track or its mounted source changes.
    /// Runs for the life of the server.
    pub async fn artist_loop(self: Arc<Self>, db: MusicDb) {
        let mut snapshots = self.subscribe();
        loop {
            let wanted = self.lock().uncredited();
            if let Some((entry_id, source)) = wanted {
                let artists = match artist_line(&db, entry_id, source.as_deref()).await {
                    Ok(artists) => artists,
                    Err(e) => {
                        tracing::warn!("looking up the artists of entry {entry_id}: {e}");
                        Vec::new()
                    }
                };
                let mut s = self.lock();
                let changed = s.queue.current().map(|c| c.entry_id) == Some(entry_id)
                    && s.credited.as_ref().is_none_or(|c| c.artists != artists);
                s.credited = Some(Credited {
                    entry_id,
                    source,
                    artists,
                    stale: false,
                });
                if changed {
                    self.publish(&mut s);
                }
                // Something else may have changed during the lookup.
                continue;
            }
            if snapshots.changed().await.is_err() {
                return;
            }
        }
    }

    /// Shows queued `entry_ids` as `title` (the entry was renamed).
    pub fn retitle(&self, entry_ids: &[i64], title: &str) {
        let mut s = self.lock();
        let mut changed = false;
        for item in &mut s.queue.items {
            if entry_ids.contains(&item.entry_id) && item.title.as_deref() != Some(title) {
                item.title = Some(title.to_owned());
                changed = true;
            }
        }
        if changed {
            self.publish(&mut s);
        }
    }

    /// Looks the current track's artists up again (an artist was renamed),
    /// keeping the old line until the new one is in.
    pub fn recredit(&self) {
        let mut s = self.lock();
        let Some(c) = s.credited.as_mut() else {
            return;
        };
        c.stale = true;
        // Wakes `artist_loop`.
        self.publish(&mut s);
    }

    fn connect(&self, device: &str, name: &str) {
        let mut s = self.lock();
        let d = s.devices.entry(device.to_owned()).or_insert(Device {
            name: String::new(),
            connections: 0,
        });
        d.connections += 1;
        d.name = name.to_owned();
        // A reload of the device that was playing picks the session back up
        // (paused — playback died with the old page).
        if s.active_device.is_none()
            && s.last_active_device.as_deref() == Some(device)
            && s.queue.current().is_some()
        {
            s.active_device = Some(device.to_owned());
        }
        self.publish(&mut s);
    }

    fn disconnect(&self, device: &str) {
        let mut s = self.lock();
        let Some(d) = s.devices.get_mut(device) else {
            return;
        };
        d.connections -= 1;
        if d.connections > 0 {
            return;
        }
        s.devices.remove(device);
        if s.active_device.as_deref() == Some(device) {
            s.set_playing(false);
            s.active_device = None;
            s.mounted = None;
        }
        self.publish(&mut s);
    }
}

struct ConnectionGuard {
    hub: Arc<PlaybackHub>,
    device: String,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.hub.disconnect(&self.device);
    }
}

/// The artist line for a track from its library credits: the artists one
/// of the sources of its soft-identity component credits (`credited_artists`), each by the name the user
/// credits it as on this track, else the canonical name of its artist
/// identity — so an auto-generated YouTube "… - Topic" channel linked to the
/// artist reads as the artist.
pub(crate) async fn artist_line(
    db: &MusicDb,
    entry_id: i64,
    mounted: Option<&str>,
) -> anyhow::Result<Vec<CreditedArtist>> {
    // Every entry soft-linked to the track: its sources are spread over
    // them (a Discogs track, the MusicBrainz recording, the YouTube upload).
    let members = crate::entries::component_members(db, entry_id).await?;
    let mut credits = Vec::new();
    for &m in &members {
        credits.extend(db.main_artist_credits(m).await?);
    }
    let ids = credited_artists(&credits, mounted);
    let summaries = crate::entries::entry_summaries(db, &ids).await?;
    let overrides = db.credit_display_names(&members).await?;
    let mut artists: Vec<CreditedArtist> = Vec::new();
    for id in &ids {
        let name = summaries.get(id).and_then(|s| {
            crate::entries::credit_name(&overrides, &members, s).or(s.title.as_deref())
        });
        if let Some(name) = name
            && !artists.iter().any(|a| a.name == name)
        {
            artists.push(CreditedArtist {
                entry_id: *id,
                name: name.to_owned(),
            });
        }
    }
    Ok(artists)
}

/// The artists one source credits as main artists, in credit order. The
/// mounted source's own credits win, then MusicBrainz's, then any other
/// source's; an uploader only when nothing lists artists.
fn credited_artists(credits: &[MainCredit], mounted: Option<&str>) -> Vec<i64> {
    let mut by_source: BTreeMap<(&str, &str), Vec<&MainCredit>> = BTreeMap::new();
    for c in credits {
        by_source
            .entry((&c.source, &c.identifier))
            .or_default()
            .push(c);
    }
    let Some((_, best)) = by_source
        .into_iter()
        .min_by_key(|((source, identifier), list)| {
            (
                list.iter().all(|c| c.role == "uploader"),
                Some(*identifier) != mounted,
                *source != "musicbrainz",
            )
        })
    else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    for c in best {
        if !ids.contains(&c.artist_entry_id) {
            ids.push(c.artist_entry_id);
        }
    }
    ids
}

/// `GET /api/playback` — the current snapshot (debugging; the frontend
/// uses the SSE feed).
pub fn snapshot(state: &Arc<AppState>) -> HyperResponse {
    respond::json(StatusCode::OK, state.playback.lock().snapshot())
}

/// `GET /api/playback/events?device_id=…&name=…` — registers the caller as
/// a device for as long as the stream stays open, and streams a full
/// snapshot (`data:` line) on every change.
pub fn events(
    state: &Arc<AppState>,
    query: &std::collections::HashMap<String, String>,
) -> HyperResponse {
    let Some(device) = query.get("device_id").filter(|d| !d.is_empty()) else {
        return respond::error(
            StatusCode::BAD_REQUEST,
            "invalid_input",
            "device_id is required",
        );
    };
    let name = query
        .get("name")
        .filter(|n| !n.is_empty())
        .map_or("Unknown device", String::as_str);
    let hub = Arc::clone(&state.playback);
    let rx = hub.tx.subscribe();
    hub.connect(device, name);
    let guard = ConnectionGuard {
        hub,
        device: device.clone(),
    };

    let stream = futures::stream::unfold((rx, guard, true), |(mut rx, guard, first)| async move {
        if first {
            let snap = rx.borrow_and_update().clone();
            let chunk = Bytes::from(format!("retry: 2000\ndata: {snap}\n\n"));
            return Some((chunk, (rx, guard, false)));
        }
        let chunk = tokio::select! {
            changed = rx.changed() => {
                changed.ok()?;
                let snap = rx.borrow_and_update().clone();
                Bytes::from(format!("data: {snap}\n\n"))
            }
            _ = tokio::time::sleep(KEEPALIVE) => Bytes::from_static(b": keepalive\n\n"),
        };
        Some((chunk, (rx, guard, false)))
    });
    let body = StreamBody::new(stream.map(|b| Ok::<_, Infallible>(Frame::data(b))));
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(body.boxed_unsync())
        .expect("response builder")
}

/// What the OS media controls can ask of the session
/// (`PlaybackHub::media_key`).
#[cfg(feature = "media_controls")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKey {
    Play,
    Pause,
    Toggle,
    Stop,
    Next,
    Prev,
    SeekTo(u64),
    /// Milliseconds, either way, clamped to the track.
    SeekBy(i64),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Command {
    Play,
    Pause,
    Toggle,
    Next,
    Prev,
    Seek {
        position_ms: u64,
    },
    Jump {
        order_index: usize,
    },
    Remove {
        order_index: usize,
    },
    SetQueue {
        items: Vec<QueueItem>,
        #[serde(default)]
        start_index: usize,
    },
    Queue {
        item: QueueItem,
    },
    /// "Shuffle all": a shuffle-mix batch the sender generated, played in
    /// its arranged order (shuffle mode off, so the mix's spacing stays).
    ShuffleLibrary {
        items: Vec<QueueItem>,
    },
    /// The active device's answer to `wants_recommend`.
    Recommend {
        items: Vec<QueueItem>,
    },
    /// Another edition of the current track, in its place.
    ReplaceCurrent {
        item: QueueItem,
    },
    ToggleShuffle,
    CycleRepeat,
    SelectSource {
        identifier: String,
    },
    Volume {
        #[serde(default)]
        global: Option<u8>,
        /// Merged into the per-provider dials (others are left alone).
        #[serde(default)]
        by_provider: BTreeMap<String, u8>,
        #[serde(default)]
        muted: Option<bool>,
        /// Only applies if the session has no volume yet — a device
        /// migrating its old per-browser settings.
        #[serde(default)]
        seed: bool,
    },
    Transfer {
        to: String,
    },
}

#[derive(Deserialize)]
struct CommandRequest {
    device_id: String,
    #[serde(flatten)]
    command: Command,
}

type CommandError = (StatusCode, &'static str);

/// `POST /api/playback/command` — `{device_id, type, ...}`.
pub fn command(state: &Arc<AppState>, body: &[u8]) -> HyperResponse {
    let req: CommandRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            return respond::error(StatusCode::BAD_REQUEST, "invalid_input", &e.to_string());
        }
    };
    let hub = &state.playback;
    let result = {
        let mut s = hub.lock();
        let r = apply(&mut s, &req.device_id, req.command);
        if r.is_ok() {
            hub.publish(&mut s);
        }
        r
    };
    match result {
        Ok(()) => respond::json(StatusCode::OK, json!({ "ok": true })),
        Err((status, message)) => respond::error(status, "invalid_input", message),
    }
}

fn apply(s: &mut Session, device: &str, command: Command) -> Result<(), CommandError> {
    const NO_TRACK: CommandError = (StatusCode::CONFLICT, "nothing is playing");
    match command {
        Command::Play => {
            if s.queue.current().is_none() {
                return Err(NO_TRACK);
            }
            s.claim_if_idle(device);
            s.set_playing(true);
        }
        Command::Pause => s.set_playing(false),
        Command::Toggle => {
            if s.queue.current().is_none() {
                return Err(NO_TRACK);
            }
            if s.playing {
                s.set_playing(false);
            } else {
                s.claim_if_idle(device);
                s.set_playing(true);
            }
        }
        Command::Next | Command::Prev => {
            let moved = if matches!(command, Command::Next) {
                s.queue.next()
            } else {
                s.queue.prev()
            };
            if !moved {
                return Err((StatusCode::CONFLICT, "no track in that direction"));
            }
            s.claim_if_idle(device);
            s.start_track(false);
        }
        Command::Seek { position_ms } => {
            if s.queue.current().is_none() {
                return Err(NO_TRACK);
            }
            s.position_ms = position_ms;
            s.position_at = Some(Instant::now());
            s.seek_seq += 1;
        }
        Command::Jump { order_index } => {
            if !s.queue.jump_to(order_index) {
                return Err((StatusCode::BAD_REQUEST, "no such queue row"));
            }
            s.claim_if_idle(device);
            s.start_track(false);
        }
        Command::Remove { order_index } => {
            if !s.queue.remove_at(order_index) {
                return Err((StatusCode::BAD_REQUEST, "can't remove that row"));
            }
        }
        Command::SetQueue { items, start_index } => {
            if items.is_empty() {
                return Err((StatusCode::BAD_REQUEST, "empty queue"));
            }
            s.queue.set(items, start_index);
            s.claim_if_idle(device);
            s.start_track(false);
        }
        Command::Queue { item } => {
            if s.queue.current().is_some() {
                s.queue.queue_next(item);
            } else {
                // Nothing to queue *behind*: a lone "Queue" click on an
                // idle player just starts it.
                s.queue.set(vec![item], 0);
                s.claim_if_idle(device);
                s.start_track(false);
            }
        }
        Command::ShuffleLibrary { items } => {
            if items.is_empty() {
                return Err((StatusCode::BAD_REQUEST, "empty queue"));
            }
            s.queue.shuffled = false;
            s.queue.set(items, 0);
            s.claim_if_idle(device);
            s.start_track(false);
        }
        Command::Recommend { items } => {
            // A late or duplicate batch (another device's, or one that
            // landed after the user queued enough) is dropped.
            if !s.needs_recommend() || items.is_empty() {
                return Ok(());
            }
            s.queue.append_post_queue(items);
            if s.advance_after_recommend.take() == Some(s.track_seq) && s.queue.next() {
                s.start_track(false);
            }
        }
        Command::ReplaceCurrent { item } => {
            if !s.queue.replace_current(item) {
                return Err(NO_TRACK);
            }
            s.claim_if_idle(device);
            s.start_track(false);
        }
        Command::ToggleShuffle => s.queue.toggle_shuffle(),
        Command::CycleRepeat => s.queue.cycle_repeat(),
        Command::SelectSource { identifier } => {
            if s.queue.current().is_none() {
                return Err(NO_TRACK);
            }
            s.source = Some(identifier);
            s.claim_if_idle(device);
            // A different source is a different recording/video: start it
            // from the top, like the old switcher did.
            s.start_track(true);
        }
        Command::Volume {
            global,
            by_provider,
            muted,
            seed,
        } => {
            if seed && s.volume.is_some() {
                return Ok(());
            }
            let v = s.volume.get_or_insert_with(|| Volume {
                global: 80,
                by_provider: BTreeMap::new(),
                muted: false,
            });
            if let Some(g) = global {
                v.global = g.min(100);
            }
            for (p, pv) in by_provider {
                v.by_provider.insert(p, pv.min(100));
            }
            if let Some(m) = muted {
                v.muted = m;
            }
        }
        Command::Transfer { to } => {
            if !s.devices.contains_key(&to) {
                return Err((StatusCode::NOT_FOUND, "no such device"));
            }
            if s.active_device.as_deref() != Some(&to) {
                s.freeze();
                s.mounted = None;
                s.active_device = Some(to.clone());
                s.last_active_device = Some(to);
            }
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct Report {
    device_id: String,
    track_seq: u64,
    #[serde(default)]
    playing: Option<bool>,
    #[serde(default)]
    position_ms: Option<u64>,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    mounted: Option<Mounted>,
    /// The track finished on its own (or failed and should be skipped).
    #[serde(default)]
    ended: bool,
}

/// `POST /api/playback/report` — the active device saying what its embed
/// is actually doing. Anything from another device, or about a track that
/// has since been replaced, is ignored (`accepted: false`).
pub fn report(state: &Arc<AppState>, body: &[u8]) -> HyperResponse {
    let r: Report = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            return respond::error(StatusCode::BAD_REQUEST, "invalid_input", &e.to_string());
        }
    };
    let hub = &state.playback;
    let accepted = {
        let mut s = hub.lock();
        let accepted =
            s.active_device.as_deref() == Some(&r.device_id) && s.track_seq == r.track_seq;
        if accepted && apply_report(&mut s, r) {
            hub.publish(&mut s);
        }
        accepted
    };
    respond::json(StatusCode::OK, json!({ "accepted": accepted }))
}

/// Folds a report into the session; whether it's worth broadcasting.
fn apply_report(s: &mut Session, r: Report) -> bool {
    if r.ended {
        if s.queue.repeat == Repeat::One {
            s.start_track(true);
        } else if s.queue.next() {
            s.start_track(false);
        } else {
            s.set_playing(false);
            if let Some(d) = s.duration_ms {
                s.position_ms = d;
            }
            s.advance_after_recommend = Some(s.track_seq);
        }
        return true;
    }
    let mut changed = false;
    if let Some(m) = r.mounted {
        s.mounted = Some(m);
        changed = true;
    }
    if r.duration_ms.is_some() && r.duration_ms != s.duration_ms {
        s.duration_ms = r.duration_ms;
        changed = true;
    }
    let expected = s.position_now();
    if let Some(p) = r.playing
        && p != s.playing
    {
        s.playing = p;
        changed = true;
    }
    if let Some(pos) = r.position_ms {
        changed |= pos.abs_diff(expected) > DRIFT_TOLERANCE_MS;
        s.position_ms = pos;
        s.position_at = Some(Instant::now());
    } else if changed {
        s.position_ms = expected;
        s.position_at = Some(Instant::now());
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: i64) -> QueueItem {
        QueueItem {
            entry_id: id,
            title: None,
            release_id: None,
        }
    }

    fn ids(q: &Queue) -> Vec<i64> {
        q.order.iter().map(|&i| q.items[i].entry_id).collect()
    }

    #[test]
    fn queue_next_lands_before_post_queue() {
        let mut q = Queue::default();
        q.set((1..=4).map(item).collect(), 0);
        q.queue_next(item(10));
        q.queue_next(item(11));
        assert_eq!(ids(&q), vec![1, 10, 11, 2, 3, 4]);
        assert_eq!(q.queue_end, 3);
        // Playing past the Queue layer: a new pick goes right after current.
        q.next();
        q.next();
        q.next();
        q.queue_next(item(12));
        assert_eq!(ids(&q), vec![1, 10, 11, 2, 12, 3, 4]);
    }

    #[test]
    fn remove_and_compact_keep_current() {
        let mut q = Queue::default();
        q.set((1..=5).map(item).collect(), 2);
        assert!(!q.remove_at(2));
        assert!(q.remove_at(0));
        assert_eq!(q.current().unwrap().entry_id, 3);
        assert_eq!(ids(&q), vec![2, 3, 4, 5]);
        assert_eq!(q.items.len(), 4);
    }

    #[test]
    fn shuffle_pins_current_and_unshuffle_restores_order() {
        let mut q = Queue::default();
        q.set((1..=20).map(item).collect(), 5);
        q.toggle_shuffle();
        assert_eq!(q.pos, 0);
        assert_eq!(q.current().unwrap().entry_id, 6);
        q.toggle_shuffle();
        assert_eq!(ids(&q), (1..=20).collect::<Vec<_>>());
        assert_eq!(q.current().unwrap().entry_id, 6);
    }

    #[test]
    fn repeat_all_wraps() {
        let mut q = Queue::default();
        q.set(vec![item(1), item(2)], 1);
        assert!(!q.next());
        q.cycle_repeat();
        assert!(q.next());
        assert_eq!(q.current().unwrap().entry_id, 1);
        assert!(q.prev());
        assert_eq!(q.current().unwrap().entry_id, 2);
    }

    #[test]
    fn history_is_trimmed() {
        let mut q = Queue::default();
        q.set((0..300).map(item).collect(), 0);
        for _ in 0..250 {
            q.next();
        }
        assert_eq!(q.pos, MAX_HISTORY);
        assert_eq!(q.current().unwrap().entry_id, 250);
        assert_eq!(q.order.len(), 300 - 150);
        assert_eq!(q.items.len(), q.order.len());
    }

    fn session_with_device(dev: &str) -> Session {
        let mut s = Session::default();
        s.devices.insert(
            dev.into(),
            Device {
                name: "x".into(),
                connections: 1,
            },
        );
        s
    }

    #[test]
    fn starting_playback_claims_idle_session() {
        let mut s = session_with_device("a");
        s.devices.insert(
            "b".into(),
            Device {
                name: "y".into(),
                connections: 1,
            },
        );
        apply(
            &mut s,
            "a",
            Command::SetQueue {
                items: vec![item(1), item(2)],
                start_index: 0,
            },
        )
        .unwrap();
        assert_eq!(s.active_device.as_deref(), Some("a"));
        assert!(s.playing);
        // A remote's command plays on the active device, not on itself.
        apply(&mut s, "b", Command::Next).unwrap();
        assert_eq!(s.active_device.as_deref(), Some("a"));
        assert_eq!(s.track_seq, 2);
    }

    #[test]
    fn the_artists_come_from_one_source_preferring_the_mounted_one() {
        let credit = |source: &str, identifier: &str, role: &str, artist: i64| MainCredit {
            source: source.into(),
            identifier: identifier.into(),
            role: role.into(),
            artist_entry_id: artist,
        };
        let credits = [
            credit("musicbrainz", "mb", "listed_artist", 1),
            credit("spotify", "sp", "listed_artist", 2),
            credit("spotify", "sp", "listed_artist", 3),
            credit("youtube", "yt", "uploader", 4),
        ];
        assert_eq!(credited_artists(&credits, Some("sp")), [2, 3]);
        // Not the mounted source's (or it credits nobody): MusicBrainz first.
        assert_eq!(credited_artists(&credits, Some("other")), [1]);
        // An uploader only when nothing lists artists.
        assert_eq!(credited_artists(&credits, Some("yt")), [1]);
        assert_eq!(credited_artists(&credits[3..], Some("yt")), [4]);
        assert!(credited_artists(&[], None).is_empty());
    }

    #[test]
    fn the_credited_artist_follows_the_track_and_its_source() {
        let mut s = session_with_device("d");
        s.queue.set(vec![item(1), item(2)], 0);
        assert_eq!(s.uncredited(), Some((1, None)));
        s.credited = Some(Credited {
            entry_id: 1,
            source: None,
            artists: vec![CreditedArtist {
                entry_id: 9,
                name: "Ado".into(),
            }],
            stale: false,
        });
        assert_eq!((s.artist(), s.uncredited()), (Some("Ado".into()), None));
        // A rename looks it up again; the old line stays meanwhile.
        s.credited.as_mut().unwrap().stale = true;
        assert_eq!(
            (s.artist(), s.uncredited()),
            (Some("Ado".into()), Some((1, None)))
        );
        s.credited.as_mut().unwrap().stale = false;
        // A mounted source has credits of its own; the old line stays meanwhile.
        s.mounted = Some(Mounted {
            provider: "spotify".into(),
            identifier: "sp".into(),
            artist: None,
            controllable: true,
        });
        assert_eq!(
            (s.artist(), s.uncredited()),
            (Some("Ado".into()), Some((1, Some("sp".into()))))
        );
        // Never another track's.
        apply(&mut s, "d", Command::Next).unwrap();
        assert_eq!((s.artist(), s.uncredited()), (None, Some((2, None))));
        assert!(s.artists().is_empty());
    }

    /// A hub with a two-track queue, `device` connected and having played
    /// it last, and nothing active (as after a reload).
    #[cfg(feature = "media_controls")]
    fn idle_hub(device: &str) -> (tempfile::TempDir, Arc<PlaybackHub>) {
        let dir = tempfile::tempdir().unwrap();
        let hub = PlaybackHub::load(dir.path().join("playback.json"));
        {
            let mut s = hub.lock();
            *s = session_with_device(device);
            s.queue.set(vec![item(1), item(2)], 0);
            s.last_active_device = Some(device.into());
            s.duration_ms = Some(60_000);
        }
        (dir, hub)
    }

    #[cfg(feature = "media_controls")]
    #[test]
    fn media_keys_resume_on_the_device_that_last_played() {
        let (_dir, hub) = idle_hub("a");
        hub.media_key(MediaKey::Play);
        let s = hub.lock();
        assert_eq!(s.active_device.as_deref(), Some("a"));
        assert!(s.playing);
    }

    #[cfg(feature = "media_controls")]
    #[test]
    fn media_keys_without_a_device_only_pause_and_seek() {
        let (_dir, hub) = idle_hub("a");
        hub.lock().devices.clear();
        for key in [MediaKey::Play, MediaKey::Toggle, MediaKey::Next] {
            hub.media_key(key);
        }
        {
            let s = hub.lock();
            assert!(!s.playing);
            assert_eq!(s.active_device, None);
            assert_eq!(s.queue.current().unwrap().entry_id, 1);
        }
        hub.media_key(MediaKey::SeekTo(5_000));
        assert_eq!(hub.lock().position_ms, 5_000);
    }

    #[cfg(feature = "media_controls")]
    #[test]
    fn relative_media_seeks_stay_inside_the_track() {
        let (_dir, hub) = idle_hub("a");
        hub.media_key(MediaKey::SeekBy(-10_000));
        assert_eq!(hub.lock().position_ms, 0);
        hub.media_key(MediaKey::SeekBy(90_000));
        assert_eq!(hub.lock().position_ms, 60_000);
    }

    #[test]
    fn ended_report_advances_or_repeats() {
        let mut s = session_with_device("a");
        apply(
            &mut s,
            "a",
            Command::SetQueue {
                items: vec![item(1), item(2)],
                start_index: 0,
            },
        )
        .unwrap();
        let ended = |seq| Report {
            device_id: "a".into(),
            track_seq: seq,
            playing: None,
            position_ms: None,
            duration_ms: None,
            mounted: None,
            ended: true,
        };
        let seq = s.track_seq;
        apply_report(&mut s, ended(seq));
        assert_eq!(s.queue.current().unwrap().entry_id, 2);
        s.queue.cycle_repeat();
        s.queue.cycle_repeat(); // one
        let seq = s.track_seq;
        apply_report(&mut s, ended(seq));
        assert_eq!(s.queue.current().unwrap().entry_id, 2);
        assert_eq!(s.track_seq, seq + 1);
        s.queue.cycle_repeat(); // off
        let seq = s.track_seq;
        apply_report(&mut s, ended(seq));
        assert!(!s.playing);
        assert_eq!(s.advance_after_recommend, Some(s.track_seq));
    }

    #[test]
    fn shuffle_library_keeps_the_mix_order() {
        let mut s = session_with_device("a");
        s.queue.shuffled = true;
        apply(
            &mut s,
            "a",
            Command::ShuffleLibrary {
                items: vec![item(1), item(2), item(3)],
            },
        )
        .unwrap();
        assert!(!s.queue.shuffled);
        assert_eq!(ids(&s.queue), vec![1, 2, 3]);
        assert_eq!(s.queue.current().unwrap().entry_id, 1);
        assert_eq!(s.active_device.as_deref(), Some("a"));
    }

    #[test]
    fn recommend_resumes_a_stalled_queue_and_drops_unneeded_batches() {
        let mut s = session_with_device("a");
        apply(
            &mut s,
            "a",
            Command::SetQueue {
                items: vec![item(1)],
                start_index: 0,
            },
        )
        .unwrap();
        assert!(s.needs_recommend());
        let seq = s.track_seq;
        apply_report(
            &mut s,
            Report {
                device_id: "a".into(),
                track_seq: seq,
                playing: None,
                position_ms: None,
                duration_ms: None,
                mounted: None,
                ended: true,
            },
        );
        assert_eq!(s.advance_after_recommend, Some(seq));
        let recommend = |ids: &[i64]| Command::Recommend {
            items: ids.iter().copied().map(item).collect(),
        };
        apply(&mut s, "a", recommend(&[2, 3, 4, 5])).unwrap();
        assert_eq!(s.queue.current().unwrap().entry_id, 2);
        assert_eq!(s.track_seq, seq + 1);
        assert!(s.playing);
        // Three still ahead: a late second batch is dropped.
        assert!(!s.needs_recommend());
        apply(&mut s, "a", recommend(&[6])).unwrap();
        assert_eq!(ids(&s.queue), vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn replace_current_swaps_in_place() {
        let mut s = session_with_device("a");
        apply(
            &mut s,
            "a",
            Command::SetQueue {
                items: vec![item(1), item(2), item(3)],
                start_index: 1,
            },
        )
        .unwrap();
        let seq = s.track_seq;
        apply(&mut s, "a", Command::ReplaceCurrent { item: item(9) }).unwrap();
        assert_eq!(ids(&s.queue), vec![1, 9, 3]);
        assert_eq!(s.queue.current().unwrap().entry_id, 9);
        assert_eq!(s.track_seq, seq + 1);
    }

    #[test]
    fn progress_within_tolerance_is_not_rebroadcast() {
        let mut s = session_with_device("a");
        apply(
            &mut s,
            "a",
            Command::SetQueue {
                items: vec![item(1)],
                start_index: 0,
            },
        )
        .unwrap();
        let progress = |pos| Report {
            device_id: "a".into(),
            track_seq: 1,
            playing: Some(true),
            position_ms: Some(pos),
            duration_ms: None,
            mounted: None,
            ended: false,
        };
        assert!(!apply_report(&mut s, progress(200)));
        assert!(apply_report(&mut s, progress(60_000)));
    }
}
