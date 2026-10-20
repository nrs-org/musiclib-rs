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
For explicit MusicBrainz recording roles, `arranger`, `instrument` and
`vocal` provide evidence for NRS's `arrange`, `inst_perform` and `vocal`
choices, respectively. Attributes can further qualify that evidence.

`source` identifies the credited entry's exact provider record.
`artist_source` identifies the artist record that assertion names. `extra`
holds the stored provider metadata, or null for older/basic assertions:

- MusicBrainz listed credits retain `credited_name`, zero-based `index`
  and nonempty `joinphrase`.
- MusicBrainz recording artist relations retain `attributes`, `direction`,
  the credited name (`target-credit` when present), and other relation facts
  such as attribute IDs, dates and role type ID.
- Discogs credits retain zero-based `index`, `credited_name` (the name
  variation, falling back to the artist name), and `joinphrase`.

Rows are ordered by source key and identifier, then credit index when
present, then insertion ID. This keeps a provider's ordered list intact;
order across providers expresses no confidence ranking. Search, resolve
and suggestion subtitles use one source's main-artist line: listed artists
first, uploaders as fallback, MusicBrainz preferred. MusicBrainz joinphrases
and entry-specific name overrides are respected.

Credits span the requested entry's soft-linked duplicates. They do not
span other editions, covers, originals or containing releases. Only targets
whose entry type is `artist` count. An empty array means no stored artist
assertions were found; it does not establish that the credit list is complete.

## Coverage and next steps

This first slice exposes existing contribution edges and preserves more of
their evidence on future imports. It makes no network requests during
resolve and requires no database migration. Re-fetch affected provider
records to populate names and attributes discarded by earlier imports.

MusicBrainz work-level composer and lyricist relations are not fetched yet.
Work IDs currently describe songs and help find original recordings; they
are not stored entities. The next fetch change should retain work credits
with their work IDs and performance context, especially for translated
lyrics, arrangements and multi-song recordings. An original recording's
performers must not become performers on its cover.

Discogs track `extraartists` are available after that track's own record is
fetched. Release-level `extraartists` and their track ranges are not yet
imported. Album staff must keep its scope when this is added. Spotify listed
artists and video uploaders remain useful identity evidence but do not
provide explicit contribution roles.

The nrs-next consumer can next group assertions by artist identity, recognize
artists through `artist.urls`, and let the user select explicit roles before
writing NRS credit relations. This change only adds musiclib's data contract;
the NRS relation suggestion and write flows still need that consumer work.
