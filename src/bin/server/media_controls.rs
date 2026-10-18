//! The session's OS media controls: one player on the machine running the
//! server (MPRIS on Linux, through `souvlaki`), showing the session's
//! track and driving it wherever it plays, a phone included.
//!
//! Browsers publish their own entry for a tab playing audio, which for an
//! embed often describes the embed alone (a YouTube video's title, no
//! queue). This one follows the session (`PlaybackHub::subscribe`) and
//! sends the OS buttons back as session commands
//! (`PlaybackHub::media_key`), so it's right whichever device plays.
//!
//! `souvlaki` also covers Windows and macOS, but neither fits a headless
//! server yet: Windows controls need a window handle, and macOS delivers
//! commands only to a main-thread run loop. Elsewhere this logs and stays
//! off.

use std::{sync::Arc, time::Duration};

use serde_json::Value;

use crate::playback::{MediaKey, PlaybackHub};

/// The parts of a snapshot the OS controls show.
#[derive(Clone, Debug, Default, PartialEq)]
struct NowPlaying {
    title: Option<String>,
    artist: Option<String>,
    art: Option<String>,
    duration_ms: Option<u64>,
    /// `None` with no current track.
    playing: Option<bool>,
    position_ms: u64,
}

impl NowPlaying {
    fn from_snapshot(snapshot: &Value) -> Self {
        let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        let current = &snapshot["current"];
        if current.is_null() {
            return Self::default();
        }
        Self {
            title: text(&current["title"]),
            // The library's credits, else what the playing source says.
            artist: text(&snapshot["artist"]).or_else(|| text(&snapshot["mounted"]["artist"])),
            art: text(&snapshot["mounted"]["art"]),
            duration_ms: snapshot["duration_ms"].as_u64().filter(|&d| d > 0),
            playing: Some(snapshot["playing"].as_bool().unwrap_or(false)),
            position_ms: snapshot["position_ms"].as_u64().unwrap_or(0),
        }
    }

    /// Whether the OS should be told new metadata (not just a new position).
    fn same_track(&self, other: &Self) -> bool {
        (&self.title, &self.artist, &self.art, self.duration_ms)
            == (&other.title, &other.artist, &other.art, other.duration_ms)
    }
}

/// The session command for an OS media-control event, if it maps to one.
fn media_key(event: souvlaki::MediaControlEvent) -> Option<MediaKey> {
    use souvlaki::{MediaControlEvent as E, MediaPosition, SeekDirection};
    const STEP_MS: i64 = 10_000;
    let signed = |dir: SeekDirection, ms: i64| match dir {
        SeekDirection::Forward => ms,
        SeekDirection::Backward => -ms,
    };
    Some(match event {
        E::Play => MediaKey::Play,
        E::Pause => MediaKey::Pause,
        E::Toggle => MediaKey::Toggle,
        E::Stop => MediaKey::Stop,
        E::Next => MediaKey::Next,
        E::Previous => MediaKey::Prev,
        E::Seek(dir) => MediaKey::SeekBy(signed(dir, STEP_MS)),
        E::SeekBy(dir, by) => MediaKey::SeekBy(signed(dir, by.as_millis() as i64)),
        E::SetPosition(MediaPosition(at)) => MediaKey::SeekTo(at.as_millis() as u64),
        E::SetVolume(_) | E::OpenUri(_) | E::Raise | E::Quit => return None,
    })
}

/// Runs the controls for the life of the server, or returns at once where
/// they can't run (see the module docs).
#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "ios", target_os = "android"))
))]
pub async fn run(hub: Arc<PlaybackHub>) {
    use souvlaki::{MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig};

    let config = PlatformConfig {
        display_name: "musiclib",
        dbus_name: "musiclib",
        hwnd: None,
    };
    let mut controls = match MediaControls::new(config) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("OS media controls unavailable: {e}");
            return;
        }
    };
    let keys = Arc::clone(&hub);
    if let Err(e) = controls.attach(move |event| {
        if let Some(key) = media_key(event) {
            keys.media_key(key);
        }
    }) {
        tracing::warn!("OS media controls unavailable: {e}");
        return;
    }
    tracing::info!("OS media controls published as \"musiclib\"");

    let mut snapshots = hub.subscribe();
    let mut shown: Option<NowPlaying> = None;
    loop {
        let snapshot = snapshots.borrow_and_update().clone();
        if let Ok(value) = serde_json::from_str::<Value>(&snapshot) {
            let now = NowPlaying::from_snapshot(&value);
            let result = (|| {
                if shown.as_ref().is_none_or(|s| !s.same_track(&now)) {
                    controls.set_metadata(MediaMetadata {
                        title: now.title.as_deref(),
                        artist: now.artist.as_deref(),
                        album: None,
                        cover_url: now.art.as_deref(),
                        duration: now.duration_ms.map(Duration::from_millis),
                    })?;
                }
                let progress = Some(MediaPosition(Duration::from_millis(now.position_ms)));
                controls.set_playback(match now.playing {
                    None => MediaPlayback::Stopped,
                    Some(true) => MediaPlayback::Playing { progress },
                    Some(false) => MediaPlayback::Paused { progress },
                })
            })();
            // The D-Bus service thread is gone (no session bus, or the
            // name is taken by another server): nothing left to update.
            if let Err(e) = result {
                tracing::warn!("OS media controls stopped: {e}");
                return;
            }
            shown = Some(now);
        }
        if snapshots.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(not(all(
    unix,
    not(any(target_os = "macos", target_os = "ios", target_os = "android"))
)))]
pub async fn run(_hub: Arc<PlaybackHub>) {
    tracing::info!(
        "OS media controls aren't available on this platform yet (Windows needs a window \
         handle, macOS a main-thread run loop)"
    );
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use souvlaki::{MediaControlEvent as E, MediaPosition, SeekDirection};

    use super::*;

    #[test]
    fn a_snapshot_shows_the_library_track_and_artist() {
        let now = NowPlaying::from_snapshot(&json!({
            "current": { "entry_id": 7, "title": "LETTER☆彡" },
            "artist": "Shirakami Fubuki",
            "mounted": { "provider": "youtube", "identifier": "x", "art": "https://a/b.jpg", "artist": "Fubuki Ch." },
            "playing": true,
            "position_ms": 1500,
            "duration_ms": 200000,
        }));
        assert_eq!(
            now,
            NowPlaying {
                title: Some("LETTER☆彡".into()),
                artist: Some("Shirakami Fubuki".into()),
                art: Some("https://a/b.jpg".into()),
                duration_ms: Some(200_000),
                playing: Some(true),
                position_ms: 1500,
            }
        );
        // Nothing mounted yet, no duration: still the title, as paused.
        let early = NowPlaying::from_snapshot(&json!({
            "current": { "entry_id": 7, "title": "LETTER☆彡" },
            "mounted": null,
            "playing": false,
            "position_ms": 0,
            "duration_ms": null,
        }));
        assert_eq!(
            (early.artist, early.duration_ms, early.playing),
            (None, None, Some(false))
        );
        assert_eq!(
            NowPlaying::from_snapshot(&json!({ "current": null })),
            NowPlaying::default()
        );
    }

    #[test]
    fn a_new_position_alone_keeps_the_metadata() {
        let a = NowPlaying {
            title: Some("t".into()),
            playing: Some(true),
            position_ms: 1,
            ..Default::default()
        };
        let b = NowPlaying {
            position_ms: 9_000,
            playing: Some(false),
            ..a.clone()
        };
        assert!(a.same_track(&b));
        assert!(!a.same_track(&NowPlaying {
            art: Some("new".into()),
            ..a.clone()
        }));
    }

    #[test]
    fn os_events_map_to_session_commands() {
        assert_eq!(media_key(E::Previous), Some(MediaKey::Prev));
        assert_eq!(
            media_key(E::Seek(SeekDirection::Backward)),
            Some(MediaKey::SeekBy(-10_000))
        );
        assert_eq!(
            media_key(E::SeekBy(SeekDirection::Forward, Duration::from_secs(5))),
            Some(MediaKey::SeekBy(5_000))
        );
        assert_eq!(
            media_key(E::SetPosition(MediaPosition(Duration::from_millis(42_000)))),
            Some(MediaKey::SeekTo(42_000))
        );
        assert_eq!(media_key(E::Raise), None);
    }
}
