#!/usr/bin/env python3
"""One-off: fill `pair_privacy` (YouTube only: every other provider's
sources count as public) and bring the HTTP cache's YouTube video
responses up to the importer's new request (`part=...,status`).

- YouTube videos and playlists: one Data API `status` request per 50 ids
  ($YOUTUBE_API_KEY). An id the answer leaves out is private or deleted,
  stored as private (as `privacy::answered` does).
- Every cached `videos?part=snippet,contentDetails&id=X` response gets that
  status spliced in and is re-keyed to `part=snippet,contentDetails,status`,
  so the next import reads it from the cache instead of spending a quota
  unit per video. A response whose new key is already cached is left alone.

Only the provider's word (`pair_privacy.provider`) is written; a manual call
made in the UI is kept. Run after a build with `pair_privacy` has opened the
DB once (the schema sync creates the table). Back up both DBs first.

    uv run scripts/backfill_privacy.py [--dry-run] [MUSICLIB_DB] [HTTP_CACHE_DB]
"""

import json
import os
import sqlite3
import sys
import time
import urllib.parse as up
import urllib.request
import zlib
from collections import Counter

args = [a for a in sys.argv[1:] if a != "--dry-run"]
DRY_RUN = "--dry-run" in sys.argv[1:]
DB = args[0] if len(args) > 0 else os.path.expanduser("~/.local/share/musiclib-rs/musiclib.db")
CACHE = args[1] if len(args) > 1 else os.path.expanduser("~/.cache/musiclib-rs/http_cache.db")
KEY = os.environ.get("YOUTUBE_API_KEY") or sys.exit("YOUTUBE_API_KEY is not set")

VIDEO = "https://youtu.be/"
PLAYLIST = "https://www.youtube.com/playlist?list="
OLD_PREFIX = "GET:https://www.googleapis.com/youtube/v3/videos?part=snippet%2CcontentDetails&id="
NEW_PREFIX = "GET:https://www.googleapis.com/youtube/v3/videos?part=snippet%2CcontentDetails%2Cstatus&id="
PRIVACIES = {"public", "unlisted", "private"}


def statuses(endpoint, ids):
    """id -> privacy for `ids`, 50 a request."""
    out = {}
    ids = sorted(set(ids))
    for i in range(0, len(ids), 50):
        chunk = ids[i : i + 50]
        query = up.urlencode({"part": "status", "id": ",".join(chunk), "key": KEY})
        with urllib.request.urlopen(f"https://www.googleapis.com/youtube/v3/{endpoint}?{query}", timeout=30) as r:
            body = json.load(r)
        listed = {it["id"]: (it.get("status") or {}).get("privacyStatus") for it in body.get("items") or []}
        for id_ in chunk:
            p = listed.get(id_, "private")
            if p in PRIVACIES:
                out[id_] = p
        print(f"  {endpoint}: {min(i + 50, len(ids))}/{len(ids)}", end="\r", file=sys.stderr)
    print(file=sys.stderr)
    return out


def decode(body, encoding):
    return zlib.decompress(body) if encoding == "zlib" else body


db = sqlite3.connect(DB, timeout=60)
if not db.execute("SELECT 1 FROM sqlite_master WHERE name = 'pair_privacy'").fetchone():
    sys.exit("pair_privacy doesn't exist yet: open the DB once with the new build (e.g. start the server).")
cache = sqlite3.connect(f"file:{CACHE}?mode=ro", uri=True) if DRY_RUN else sqlite3.connect(CACHE, timeout=60)

pairs = {(s, i) for s, i in db.execute("SELECT source, identifier FROM entry_source")}
db_videos = {i[len(VIDEO) :] for s, i in pairs if s == "youtube" and i.startswith(VIDEO)}
db_playlists = {i[len(PLAYLIST) :] for s, i in pairs if s == "youtube" and i.startswith(PLAYLIST)}
cached = cache.execute(
    "SELECT key FROM cache_entry WHERE substr(key, 1, ?) = ? AND status = 200", (len(OLD_PREFIX), OLD_PREFIX)
).fetchall()
cached = [k for (k,) in cached]
cached_ids = {k[len(OLD_PREFIX) :]: k for k in cached}
print(f"{len(db_videos)} library videos, {len(db_playlists)} library playlists, {len(cached_ids)} cached video responses")

video_privacy = statuses("videos", db_videos | set(cached_ids))
playlist_privacy = statuses("playlists", db_playlists)

# --- provider privacy for library pairs ---

rows = []
for id_ in db_videos:
    if id_ in video_privacy:
        rows.append(("youtube", VIDEO + id_, video_privacy[id_]))
for id_ in db_playlists:
    if id_ in playlist_privacy:
        rows.append(("youtube", PLAYLIST + id_, playlist_privacy[id_]))
counts = Counter(("playlist" if i.startswith(PLAYLIST) else "video", p) for _, i, p in rows)
for (kind, privacy), n in sorted(counts.items()):
    print(f"{kind:9} {privacy:9} {n:6}")

# --- the cache's video responses, re-keyed with their status ---

taken = {
    k
    for (k,) in cache.execute(
        "SELECT key FROM cache_entry WHERE substr(key, 1, ?) = ?", (len(NEW_PREFIX), NEW_PREFIX)
    )
}
rekeys = []
for id_, old in cached_ids.items():
    new = NEW_PREFIX + id_
    if new in taken or id_ not in video_privacy:
        continue
    body, encoding = cache.execute("SELECT body, body_encoding FROM cache_entry WHERE key = ?", (old,)).fetchone()
    try:
        response = json.loads(decode(body, encoding))
    except ValueError:
        continue
    for item in response.get("items") or []:
        item["status"] = {"privacyStatus": video_privacy[id_]}
    raw = json.dumps(response, ensure_ascii=False).encode()
    rekeys.append((zlib.compress(raw) if encoding == "zlib" else raw, new, old))
print(f"cache: {len(rekeys)} video responses to re-key ({len(taken)} already under the new key)")

if DRY_RUN:
    print("dry run: nothing written")
    sys.exit(0)

now = int(time.time())
db.executemany(
    """
    INSERT INTO pair_privacy (source, identifier, provider, manual, checked_at) VALUES (?, ?, ?, NULL, ?)
    ON CONFLICT (source, identifier) DO UPDATE SET provider = excluded.provider, checked_at = excluded.checked_at
    """,
    [(s, i, p, now) for s, i, p in rows],
)
db.commit()
cache.executemany("UPDATE cache_entry SET body = ?, key = ? WHERE key = ?", rekeys)
cache.commit()
print("pair_privacy now:", dict(db.execute("SELECT provider, COUNT(*) FROM pair_privacy GROUP BY provider").fetchall()))
