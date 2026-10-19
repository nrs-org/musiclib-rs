#!/usr/bin/env python3
"""One-off: fill `pair_image` from the HTTP cache (docs/plan-images.md).

Every response the importer ever fetched is still in `http_cache.db`
(expired entries are never deleted), so the pictures providers returned can
be read back without any requests. The picks mirror the Rust extraction
(`Image::pick`, `snippet_thumbnail`, `primary_image`, `info_thumbnail`,
`cover_art_archive_front`).

Run after a build with `pair_image` has opened the DB once (the schema sync
creates the table). Back up the DB first.

    uv run scripts/backfill_images.py [MUSICLIB_DB] [HTTP_CACHE_DB]
"""

import json
import os
import re
import sqlite3
import sys
import urllib.parse as up
import zlib
from collections import Counter

DB = sys.argv[1] if len(sys.argv) > 1 else os.path.expanduser("~/.local/share/musiclib-rs/musiclib.db")
CACHE = sys.argv[2] if len(sys.argv) > 2 else os.path.expanduser("~/.cache/musiclib-rs/http_cache.db")
TARGET = 300


# --- picks (same rules as the Rust extraction) ---


def pick(variants):
    """Smallest variant with a long side >= 300, else the largest sized one,
    else the first. `variants` are (url, width, height)."""
    variants = [v for v in variants if v[0]]
    side = lambda v: max(v[1] or 0, v[2] or 0) if (v[1] or v[2]) else None
    big = [v for v in variants if side(v) is not None and side(v) >= TARGET]
    if big:
        return min(big, key=side)
    sized = [v for v in variants if side(v) is not None]
    if sized:
        return max(sized, key=side)
    return variants[0] if variants else None


def spotify_images(obj):
    return pick([(i.get("url"), i.get("width"), i.get("height")) for i in obj.get("images") or []])


def youtube_thumbnail(snippet):
    thumbs = (snippet or {}).get("thumbnails") or {}
    return pick([(t.get("url"), t.get("width"), t.get("height")) for t in thumbs.values()])


def discogs_primary(obj):
    images = obj.get("images") or []
    image = next((i for i in images if i.get("type") == "primary"), images[0] if images else None)
    if not image:
        return None
    return pick(
        [v for v in [(image.get("uri"), image.get("width"), image.get("height")), (image.get("uri150"), 150, 150)] if v[0]]
    )


def ytdlp_thumbnail(info):
    thumbs = [t for t in info.get("thumbnails") or [] if t.get("url") and "?" not in t["url"]]
    sized = pick([(t["url"], t.get("width"), t.get("height")) for t in thumbs if t.get("width")])
    if sized and max(sized[1] or 0, sized[2] or 0) >= TARGET:
        return sized
    unsized = [t for t in thumbs if not t.get("width")]
    if unsized:
        best = max(unsized, key=lambda t: t.get("preference", float("-inf")))
        return (best["url"], None, None)
    return sized


def caa_front(release):
    if (release.get("cover-art-archive") or {}).get("front"):
        return (f"https://coverartarchive.org/release/{release['id']}/front-250", 250, 250)
    return None


# --- the cache ---

cache = sqlite3.connect(f"file:{CACHE}?mode=ro", uri=True)


def body(key):
    row = cache.execute("SELECT body, body_encoding FROM cache_entry WHERE key = ? AND status = 200", (key,)).fetchone()
    if not row:
        return None
    raw = zlib.decompress(row[0]) if row[1] == "zlib" else row[0]
    try:
        return json.loads(raw)
    except ValueError:
        return None


# (kind, id) -> cache key of the response holding it. YouTube keys can be
# batched (`videos?id=a,b,c`) and channels may be keyed by handle.
lookup = {}
listings = []  # (kind, key)
for (key,) in cache.execute("SELECT key FROM cache_entry WHERE status = 200"):
    url = key.split(":", 1)[1]
    p = up.urlsplit(url)
    q = up.parse_qs(p.query)
    if p.netloc == "api.spotify.com":
        if m := re.fullmatch(r"/v1/(albums|artists|playlists)/(\w+)", p.path):
            lookup[("spotify:" + m[1], m[2])] = key
        elif re.fullmatch(r"/v1/artists/\w+/albums", p.path):
            listings.append(("spotify:artist_albums", key))
    elif p.netloc == "www.googleapis.com" and p.path.startswith("/youtube/v3/"):
        kind = p.path.rsplit("/", 1)[1]
        for i in ",".join(q.get("id", [])).split(","):
            if i:
                lookup[("youtube:" + kind, i)] = key
        if kind == "channels" and "forHandle" in q:
            lookup[("youtube:handle", q["forHandle"][0].lstrip("@").lower())] = key
        if kind == "playlists" and "channelId" in q:
            listings.append(("youtube:playlists", key))
        if kind == "playlistItems":
            listings.append(("youtube:playlistItems", key))
    elif p.netloc == "api.discogs.com":
        if m := re.fullmatch(r"/(releases|masters|artists)/(\d+)", p.path):
            lookup[("discogs:" + m[1], m[2])] = key
        elif re.fullmatch(r"/artists/\d+/releases", p.path):
            listings.append(("discogs:artist_releases", key))
    elif re.fullmatch(r"/ws/2/release/[0-9a-f-]{36}", p.path):
        lookup[("musicbrainz:release", p.path.rsplit("/", 1)[1])] = key
    elif p.path.startswith("/entities/"):
        lookup[("ytdlp", up.unquote(p.path[len("/entities/") :]))] = key


def own_image(source, ident):
    """The pair's picture from its own cached response, or None."""
    if source == "spotify":
        if m := re.fullmatch(r"https://open\.spotify\.com/(album|artist|playlist)/(\w+)", ident):
            b = body(lookup.get((f"spotify:{m[1]}s", m[2]), ""))
            return b and spotify_images(b)
    elif source == "youtube":
        for pattern, kind in [
            (r"https://youtu\.be/(.+)", "videos"),
            (r"https://www\.youtube\.com/playlist\?list=(.+)", "playlists"),
            (r"https://www\.youtube\.com/channel/(.+)", "channels"),
        ]:
            if m := re.fullmatch(pattern, ident):
                b = body(lookup.get((f"youtube:{kind}", m[1]), ""))
                item = next((i for i in (b or {}).get("items", []) if i.get("id") == m[1]), None)
                return item and youtube_thumbnail(item.get("snippet"))
        if m := re.fullmatch(r"https://www\.youtube\.com/@(.+)", ident):
            b = body(lookup.get(("youtube:handle", up.unquote(m[1]).lower()), ""))
            items = (b or {}).get("items") or []
            return items and youtube_thumbnail(items[0].get("snippet"))
    elif source == "discogs":
        if m := re.fullmatch(r"https://www\.discogs\.com/(release|master|artist)/(\d+)", ident):
            b = body(lookup.get((f"discogs:{m[1]}s", m[2]), ""))
            return b and discogs_primary(b)
    elif source == "musicbrainz":
        if m := re.fullmatch(r"https://musicbrainz\.org/release/([0-9a-f-]{36})", ident):
            b = body(lookup.get(("musicbrainz:release", m[1]), ""))
            return b and caa_front(b)
    elif source in ("nicovideo", "soundcloud"):
        b = body(lookup.get(("ytdlp", ident), ""))
        return b and ytdlp_thumbnail(b)
    return None


def listed_images():
    """(source, identifier, image) for every child a cached listing pictures."""
    for kind, key in listings:
        b = body(key) or {}
        if kind == "spotify:artist_albums":
            for a in b.get("items") or []:
                yield "spotify", f"https://open.spotify.com/album/{a['id']}", spotify_images(a)
        elif kind == "youtube:playlists":
            for pl in b.get("items") or []:
                yield "youtube", f"https://www.youtube.com/playlist?list={pl['id']}", youtube_thumbnail(pl.get("snippet"))
        elif kind == "youtube:playlistItems":
            for it in b.get("items") or []:
                video = ((it.get("snippet") or {}).get("resourceId") or {}).get("videoId")
                if video:
                    yield "youtube", f"https://youtu.be/{video}", youtube_thumbnail(it.get("snippet"))
        elif kind == "discogs:artist_releases":
            for r in b.get("releases") or []:
                if r.get("thumb") and r.get("type") in ("release", "master"):
                    yield "discogs", f"https://www.discogs.com/{r['type']}/{r['id']}", (r["thumb"], 150, 150)


# --- write ---

# The server may be running and writing: wait for its locks.
db = sqlite3.connect(DB, timeout=60)
if not db.execute("SELECT 1 FROM sqlite_master WHERE name = 'pair_image'").fetchone():
    sys.exit("pair_image doesn't exist yet: open the DB once with the new build (e.g. start the server).")

UPSERT = """
INSERT INTO pair_image (source, identifier, url, width, height, origin, run_id)
VALUES (?, ?, ?, ?, ?, ?, NULL)
ON CONFLICT (source, identifier) DO UPDATE
SET url = excluded.url, width = excluded.width, height = excluded.height, origin = excluded.origin
WHERE excluded.origin = 'fetch' OR pair_image.origin = 'listing'
"""

pairs = {(s, i) for s, i in db.execute("SELECT source, identifier FROM entry_source")}
counts = Counter()
for source, ident in sorted(pairs):
    img = own_image(source, ident)
    if img:
        db.execute(UPSERT, (source, ident, img[0], img[1], img[2], "fetch"))
        counts[(source, "fetch")] += 1
for source, ident, img in listed_images():
    if img and (source, ident) in pairs:
        db.execute(UPSERT, (source, ident, img[0], img[1], img[2], "listing"))
        counts[(source, "listing")] += 1
db.commit()

for (source, origin), n in sorted(counts.items()):
    print(f"{source:12} {origin:8} {n:6}")
total = db.execute("SELECT origin, COUNT(*) FROM pair_image GROUP BY origin").fetchall()
print("pair_image now:", dict(total))
