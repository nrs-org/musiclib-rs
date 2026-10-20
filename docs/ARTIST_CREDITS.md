# Artist credits for NRS

`GET /nrs-store/v1/entries/{store_id}` includes `extra.credits`, an array of
the artist credits musiclib currently knows for that entry. This is an
additive, musiclib-specific field on Store Protocol 0.5. NRS can use it to
show credit evidence and prepare artist bindings and role choices.

```json
{
  "extra": {
    "credits": [
      {
        "artist": {
          "store_id": "17",
          "title": "Artist's display name",
          "kind": "artist",
          "subtitle": null,
          "release_date": null,
          "canonical_url": "https://musicbrainz.org/artist/...",
          "urls": ["https://musicbrainz.org/artist/..."],
          "edition_group": null
        },
        "name": "Name credited on this recording",
        "credited_name": "Name credited on this recording",
        "renamed": false,
        "role": "instrument",
        "main_artist": false,
        "source": {
          "source": "musicbrainz",
          "identifier": "https://musicbrainz.org/recording/..."
        },
        "artist_source": {
          "source": "musicbrainz",
          "identifier": "https://musicbrainz.org/artist/..."
        },
        "extra": {
          "credited_name": "Name credited on this recording",
          "attributes": ["guitar"],
          "direction": "backward"
        }
      }
    ]
  }
}
```

Each row is one provider assertion. `artist` is a protocol entry summary;
its store ID is the least ID in the artist's current soft-identity
component, and its URLs span that component. Multiple assertions can refer
to that same identity. Keep their roles and source evidence when grouping
them for display. Like other store IDs, the ID is opaque to clients and may
change as entries are merged; URLs help recognize existing bindings.

`name` prefers the user's credit-name override for this entry and artist,
then the provider's credited name, then the artist's display title.
`credited_name` retains the provider's name even when an override exists;
it is null if the stored assertion has no such fact. `renamed` says whether
`name` came from an override. Artist display names and aliases are separate
from names used on individual credits.

`role` is the provider's role string. `main_artist` means the provider lists
the artist as a main artist; an uploader can also have this flag. Neither
fact establishes a composition, performance or lyrics credit. NRS owns its
role expressions and weights: it should preserve unfamiliar roles for review
and avoid converting `listed_artist` or `uploader` into a role expression.
For explicit MusicBrainz roles, `composer`, `lyricist`, `arranger`,
`instrument` and `vocal` provide evidence for NRS's `compose`, `lyrics`,
`arrange`, `inst_perform` and `vocal` choices, respectively. Attributes and
performance context can further qualify that evidence. Generic `writer`
and `translator` roles remain as stated by the provider.

`source` identifies the credited entry's exact provider record.
`artist_source` identifies the artist record that assertion names. `extra`
holds the stored provider metadata, or null for older/basic assertions:

- MusicBrainz listed credits retain `credited_name`, zero-based `index`
  and nonempty `joinphrase`.
- MusicBrainz recording artist relations retain `attributes`, `direction`,
  the credited name (`target-credit` when present), and other relation facts
  such as attribute IDs, dates and role type ID.
- Discogs credits retain zero-based `index`, `credited_name` (the name
  variation, falling back to the artist name), `joinphrase` and `tracks`.
- MusicBrainz work artist relations also carry `scope: "work"`, the work's
  `id`, `title`, `url`, `language` and `languages` under `work`, and the
  recording-to-work relation's qualifiers and dates under `performance`.
- Discogs release staff carry `scope: "release"`, the release URL/title
  under `release`, and the raw selector under `tracks`. A credit projected
  to an explicitly selected track also carries its `track_position`.

Rows are ordered by source key and identifier, then credit index when
present, then insertion ID. This keeps a provider's ordered list intact;
order across providers expresses no confidence ranking. Search, resolve
and suggestion subtitles use one source's main-artist line: listed artists
first, uploaders as fallback, MusicBrainz preferred. MusicBrainz joinphrases
and entry-specific name overrides are respected.

Credits span the requested entry's soft-linked duplicates. Other editions
and originals are not sources of inherited performer credits. Work credits
are projected only from works the recording explicitly performs. Discogs
release credits are projected only through explicit matching track selectors.
Only targets whose entry type is `artist` count. An empty array means no
stored artist assertions were found; it does not establish that the credit
list is complete.

## Import coverage

Resolve reads stored contribution edges, makes no network requests, and
requires no database migration. Re-fetch affected provider records to add
newly supported credits and populate metadata discarded by earlier imports.

Each MusicBrainz recording import looks up its distinct performed works
using `work/{id}?inc=artist-rels+work-rels`. Composer, lyricist, writer,
translator, arranger and other explicit work artist roles become non-main
artist contributions with work and performance provenance. Works remain
metadata, rather than stored entries.

The same work response supplies song grouping and arrangement-source links.
Work credit lookup runs even with `resolve_original_recordings: false`.
Only directly performed works supply credits: linked original works,
translations and recordings are not recursively visited for credit
inheritance. Medleys keep credits from all performed works with separate
work IDs; repeated work relationships retain distinct performance context.
Instrumental, partial, cover and medley flags are evidence for the consumer
to interpret, without automatically assigning full-song credit shares.

A work that returns 404 contributes no credits. Other lookup or decoding
failures fail the recording fetch, so a transient failure cannot turn into
an apparently successful refresh that prunes known writing credits. These
requests use the configured MusicBrainz base URL, HTTP cache and scheduler.

Discogs release `extraartists` become release artist credits. When a track
is fetched, its own `extraartists` and explicitly matching release credits
are imported. Comma-separated positions and inclusive `A1 to B2` ranges are
matched against actual tracklist order, including multi-disc positions.
Blank selectors, unmatched ranges and ambiguous/repeated positions stay
as release-level evidence. Unlinked Discogs credits (`id: 0`) do not create
artist identities. Spotify listed artists and video uploaders remain useful
identity evidence but do not provide explicit contribution roles.

The MusicBrainz lookup and relationship fields follow the
[API documentation](https://musicbrainz.org/doc/MusicBrainz_API#Relationships).
The real `work_beautiful_circle.json` fixture was recorded from
[the work's artist and work relations](https://musicbrainz.org/ws/2/work/449529aa-9206-48da-85e5-07e663cfb494?fmt=json&inc=artist-rels%2Bwork-rels).
Discogs selector handling follows its
[credit guidelines](https://support.discogs.com/hc/en-us/articles/360005006834-Database-Guidelines-10-Credits).

## NRS consumer

The nrs-next consumer can next group assertions by artist identity, recognize
artists through `artist.urls`, and let the user select explicit roles before
writing NRS credit relations. This change only adds musiclib's data contract;
the NRS relation suggestion and write flows still need that consumer work.
