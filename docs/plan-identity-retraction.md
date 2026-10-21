# Retracting identity links

Status: links are recorded and retracted; splitting is implemented behind
`setting.split_retracted_links = '1'`, off until a DB's links are backfilled.

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

With splitting on, an entry holding a barrier pair that a flush touches is
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

## Why it can't split yet

Connectivity needs *every* link inside an entry, and none of today's are
recorded. Recording them on re-fetch isn't enough: a link inside entry E
can be asserted by a pair outside it (an album listing that gives one track
both a Spotify ID and an ISRC). E's own pairs can all be re-fetched while
that album isn't, and E would look disconnected and split wrongly.

So:

1. **Record and retract links, no splitting** (done). No behaviour change;
   the table fills as re-imports run.
2. **Backfill** by re-importing every fetched pair, walking non-artist
   listings (cache-served mostly, as in the 2026-10-21 credit backfill).
   Open question: links only an artist discography listing states are not
   re-walked by `no_fetch_discography.yaml`; measure how many entries
   depend on them before trusting the backfill.
3. **Report** the entries that would split given the recorded links
   (`dedup`-style binary, read-only), and review it.
4. **Enable splitting**, behind a DB-level marker set by the backfill, so a
   DB without one never splits.

## Aside

`upsert_relation` re-enables a relation on conflict, so a tombstoned
provider relation comes back on the next import that lists it.
