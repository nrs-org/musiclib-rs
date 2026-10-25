# Last.fm scrobbling

Open the player's **Settings → Last.fm**. Create an API application at
<https://www.last.fm/api/account/create>, save its API key and shared secret,
then choose **Connect Last.fm**. Follow **Authorize on Last.fm** and return to
choose **Finish connecting**. The desktop authorization flow does not need a
callback URL. The login is shared by all playback devices.

Scrobbling starts with plays begun after the account is first connected;
existing history is not automatically uploaded. Disconnecting stops submissions.
Reconnecting the same account resumes its pending submissions.

## Eligibility and metadata

The track must be longer than 30 seconds, and listening must reach half its
duration or four minutes, whichever is earlier. Pauses and seeks do not add
listening time. A scrobble uses the original play start timestamp, and is sent
once when eligible; it need not wait until the track ends.

Metadata is selected from the played track's identity, including its soft-linked
duplicates. A complete Spotify track title, primary listed artist and duration
is preferred, then a complete MusicBrainz recording. These fields come from the
same catalog pair. An available album from that pair's release is included.
The primary listed artist is used for tracks with multiple artists.

When the same identity has a MusicBrainz recording link, its recording MBID is
included in both scrobbles and Now Playing updates, even when Spotify supplies
the names and duration. IDs from related covers, instrumentals or other editions
are excluded. Existing queued payloads without an MBID remain readable and keep
their original metadata when retried.

YouTube video titles and channel names are never used as scrobble metadata.
A linked music video therefore scrobbles its Spotify/MB track even while playing
on YouTube. Without usable catalog metadata, the play stays local and can become
eligible after metadata is imported. Covers, live takes and instrumentals are
separate identities: scrobbling never folds them into the edition group's default.

## Persistence and failures

The library DB keeps a `scrobble_submission` outbox, keyed by Last.fm account
and play ID. Metadata and timestamp are frozen before the first request.
Accepted, ignored and permanently rejected submissions are recorded; transient
failures retry with backoff, oldest first, including after server restarts.
An invalid session requires reconnecting. Last.fm's ignored responses are
inspected even when HTTP reports success. Daily-limit responses remain pending.
Now Playing is best effort, refreshed on the next 15-second worker tick for a
new track, and never queued for retry.

Credentials and session keys are saved in `lastfm.json` beside `musiclib.db`,
with owner-only permissions on Unix. The shared secret and session key are never
returned by the status API or stored in browser localStorage. The authorization
link carries the public API key and a short-lived request token. The Settings
panel shows submission counts and the latest upstream error. The API is under
`/api/lastfm/`:

- `GET status`
- `PUT config` with `{ "api_key": "…", "secret": "…" }`
- `POST authorize` returns an authorization URL
- `POST connect` exchanges the server-held authorized token for a session
- `DELETE session` disconnects

Playback snapshots now include a persistent `play_key`, renewed for each new
track or replay. `POST /api/plays` accepts it as `session_key` and resumes the
existing row across reloads and device transfers. Progress reports include a
unique `reporter` for each engine mount; cumulative reports are idempotent per
reporter and summed into the play, preserving late reports from a former device.
Legacy clients can still use the original start/progress API.

References: [scrobbling rules and error handling](https://www.last.fm/api/scrobbling),
[scrobble parameters](https://www.last.fm/api/show/track.scrobble),
[desktop authentication](https://www.last.fm/api/desktopauth).

## Development checks

Run `cargo test --bin server`, `cargo test --lib musicdb::`, and
`node scripts/test-play-log.mjs`. Last.fm HTTP integration tests use a local
mock endpoint and never submit plays to a real account.
