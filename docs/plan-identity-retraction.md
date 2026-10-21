# Retracting identity links

Status: implemented. Splitting runs once a DB's links are complete
(`setting.links_complete = '1'`).

A re-fetch already replaces what each fetched pair owns: child edges and
credits (non-artist parents), aliases (every pair), provider
cover/remix/arrangement relations (through their child edges), and a 404
marks the pair `gone_at`. Identity is the one fact that still only grows.

## The problem

`is_rel` links (a fetched pair's own `sources`, plus the sibling pairs a
parent's listing gives one child) are not stored. `flush` turns them into
shared `entry_source.entry_id`s and forgets them, and step 3 unions every
pair that already shares an entry. So when MusicBrainz drops a wrong
Spotify link, the two recordings stay one entry forever, and nothing says
which link merged them. Dedup barriers are the only way to split them.

## Model

`pair_link(a, b, asserted_by, via, run_id)`, `a < b`, one row per pair that
stated the link:

- `via = 'fetch'`: in `asserted_by`'s own fetch result (`importer` step 6,
  including IDs other providers' `resolve_external_source` added). Replaced
  whenever `asserted_by` is fetched, artists included.
- `via = 'listing'`: sibling pairs of one child in `asserted_by`'s listing
  (`importer` step 7). Replaced only when `asserted_by` is a fully fetched
  non-artist parent, like child edges; an artist's policy-filtered listing
  claims nothing.

Rows are written in `flush` tagged with `run_id` (and purged with the run),
retracted in `commit_import_run` from a per-asserter "current" set in
`Replacements`, exactly like child edges.

Splitting happens in the final flush, where the barrier is compiled: for
an entry in which a link this run retracted is no longer stated by anything
(`regrouped_entries`), its pairs are unioned by its remaining links, the
run's fresh `is_rel` and barrier anchors, instead of "everything sharing
the entry". The existing barrier-split path then does the rest: the class
with the entry's oldest pairs keeps the id (`split_keepers`), the others
get fresh entries with the old entry's type, pairs are re-pointed, emptied
entries are GC'd. That entry's dead links are deleted in the same flush,
so a run rolled back afterwards leaves the split and its links agreeing
(like a barrier split, a final flush's split isn't undone by rollback).

With links complete, an entry holding a barrier pair that a flush touches is
grouped by its links too (and joins the flush whole either way, so a merge
can't carry an anchor pair the barrier never saw). A pair both anchors
share then takes only what links to it to the anchor that reaches it
first, instead of the whole entry; a pair nothing inside such an entry
links to stays with its largest group.

Only a retraction triggers a split. An entry that is already disconnected
(its wrong link was removed upstream before links were recorded) stays
merged until something splits it by hand.

Entry-keyed data stays with the keeper, as it does for barrier splits:
`play_event` (it has no pair, so the played half is unknown),
`entry_display_name`, `preferred_edition`, `credit_display_name`, soft
relations and `dedup_feedback`. Pair-keyed data (credits, aliases,
images, privacy) moves with its pair. NRS binds by URL, so its references
survive.

## When links are complete

Connectivity needs *every* link inside an entry. With some missing,
entries that are fine look disconnected, and the first retraction in one
would split them. So the regrouping above only runs once
`setting.links_complete = '1'`:

- A DB created empty gets it from its migration: every link is recorded
  from its first import.
- A DB that had pairs before `pair_link` existed gets `'0'`, until
  `import --backfill-links` re-imports every fetched pair (listing
  children, fetching none), retries the ones that fail once, and sets it.

On the 2026-10-21 DB the backfill took 3 minutes from the HTTP cache and
recorded 82,184 links. No link came from a listing (`via = 'listing'`):
a child's sibling pairs are rare in practice, so a track's ISRC link comes
from the track's own fetch. Of 12,139 entries with 2+ pairs, 2 weren't
connected by their links: a merged-away MusicBrainz ID that now 404s (kept
by the gone-pair rule) and an artist URL a barrier holds.

## Aside

`upsert_relation` re-enables a relation on conflict, so a tombstoned
provider relation comes back on the next import that lists it.
