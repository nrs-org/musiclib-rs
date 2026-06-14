# musiclib-rs

Metadata storage for music.

## Motivation

Basically we have N video sharing platforms and M music streaming services.
Professionally licensed music is *mostly* available on Spotify and others but
everything else goes into YouTube or in some case, Niconico. Sometimes the song
is only officially released on SoundCloud and random osu! players reuploaded
that to YouTube.

[plst4](https://github.com/btmxh/plst4) was born as a media playback service
using these platforms' playback APIs. However, the system is not designed
exclusively for music, so it was nothing but a web player. `musiclib-rs` aims to
fill the remaining gap: to store and process music metadata efficiently while
keeping track with upstream APIs.

## Usage

`musiclib-rs` is highly configurable. The HTTP client (which controls caching,
retrying, etc.) and API providers can be configured by modifying
`$CONFIG_DIR/musiclib-rs/{http,providers}.yaml` (where `$CONFIG_DIR` is
your system config dir, look it up). See [docs/CONFIG_REFERENCE.md](docs/CONFIG_REFERENCE.md)
for a full reference of all config files, and [docs/DEV.md](docs/DEV.md) for
setup instructions, binary usage, and development workflow.

### Self-hosting

Default fetching can be very slow. One major bottleneck is Musicbrainz
`resolve_external_source` (maps non-MB URLs to a MB URL). This can be sped up by
building an URL cache to eliminate false-positives. Run the `mb_extract_urls` to
build a URL cache database in `$DATA_DIR/musiclib-rs/mb_mirror.db`. This must be
maintained to be in-sync with the remote MB server, which can be done by
triggering the replication process `mb_sync_replication`.
This is much faster than performing replication on a self-hosted MB database.

If you want to go further, you can
self-host Musicbrainz by following the instructions in the
[musicbrainz-docker](https://github.com/metabrainz/musicbrainz-docker) repo.
Currently searching is not required, so you can skip setting up search indexes.
Even then, the storage requirement is still considerable (approximately 100GB).
Once finished, replace the base URL of Musicbrainz with the locally-hosted
endpoint. Make sure that your self-hosted DB is kept up-to-date using periodic
replication.

As for Discogs, we currently don't support self-hosting that. The platform does
not have a replication process to keep metadata up-to-date, neither is there a
API-compatible service for us to query from. Discogs is rarely a bottleneck that
makes the effort of supporting self-hosted not really necessary.

### Fetch options

As of right now, the main binary of `musiclib-rs` is `import`, which
automatically import entries from a source URL. This URL can be from any source
(MB `resolve_external_source` could resolve it to the canonical MB URL), but
prefer something that links to other URLs (e.g. Discogs, MB URLs).
The binary accepts a fetch options YAML file, which controls how **child entries**
are fetched.

We provide three sample fetch options in the `config/fetch_options` directory,
mainly targeted to artists. `fetch_discography.yaml` recursively fetches all
discographies of a given artist. `vtuber_fetch_discography.yaml` replaces
fetching YouTube videos (as there can be a lot of those) by song YouTube
playlists (best-effort matching via manual regular expressions).
`no_fetch_discography.yaml` fetches no discography. It is important to note that
the fetch discography rules, as defined in these files, only applies to the root
entry to prevent infinite loops.

It is advised to fetch as many children from an entry as possible to prevent an
entry from being in an **incomplete** state. As this is impossible without
pretty much fetching everything from remote APIs, a good tradeoff is allowing
artists entry to be incomplete, which means there will be only artist entries
with partial information (their discographies are not fully-available).
Incomplete tracks lead to incorrect artist attributions (WIP), and incomplete
releases breaks music recommendation.
