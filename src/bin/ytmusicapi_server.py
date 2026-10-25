#!/usr/bin/env -S uv run
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "ytmusicapi>=1.7.0",
# ]
# ///
"""Minimal HTTP server exposing ytmusicapi artist data.

Endpoints:
  GET /artists/<channel_id>
    Calls ytmusicapi.YTMusic().get_artist(channel_id) and returns the JSON response.
    <channel_id> must be a YouTube channel ID starting with "UC".
  GET /search?q=<query>[&limit=<n>]
    Songs, then videos, matching <query>, flattened to the fields the player's
    source finder shows (see musiclib-rs's `find_source`).

Usage:
  PORT=9001 uv run src/bin/ytmusicapi_server.py

HOST picks the address to bind (default 0.0.0.0, every interface).

The musiclib-rs youtube_api channel provider reads YTMUSICAPI_SERVER_URL and calls
this server to supplement the Data API v3 channel fetch with YTMusic discography
(albums and singles) that the official API does not expose.
"""

import json
import os
import sys
import urllib.parse
from http.server import BaseHTTPRequestHandler, HTTPServer

import ytmusicapi

yt = ytmusicapi.YTMusic()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, format, *args):  # noqa: A002
        print(f"[ytmusicapi_server] {self.address_string()} - {format % args}", file=sys.stderr)

    def do_GET(self):
        parsed = urllib.parse.urlparse(self.path)
        path = parsed.path
        query = urllib.parse.parse_qs(parsed.query)

        if path.startswith("/artists/"):
            rest = path[len("/artists/"):]
            parts = rest.split("/", 1)
            channel_id = urllib.parse.unquote(parts[0])
            if not channel_id:
                self._send_error(400, "missing channel_id")
                return

            # GET /artists/{id}  →  get_artist
            if len(parts) == 1:
                try:
                    data = yt.get_artist(channel_id)
                    self._send_json(200, json.dumps(data).encode())
                except Exception as exc:
                    self._send_error(500, str(exc))

            # GET /artists/{id}/discography
            # Returns a flat list of all albums+singles, resolving pagination internally.
            elif parts[1] == "discography":
                try:
                    try:
                        artist = yt.get_artist(channel_id)
                    except KeyError as exc:
                        # A channel without a YouTube Music artist page gets
                        # a plain channel page (musicVisualHeaderRenderer:
                        # Videos and Playlists shelves only), which
                        # get_artist can't parse. It has no albums or singles.
                        if exc.args != ("musicImmersiveHeaderRenderer",):
                            raise
                        self._send_json(200, b"[]")
                        return
                    results = []
                    for section_key in ("albums", "singles"):
                        section = artist.get(section_key) or {}
                        params = section.get("params")
                        if params:
                            try:
                                results.extend(yt.get_artist_albums(section["browseId"], params))
                            except Exception as exc:
                                print(f"[ytmusicapi_server] get_artist_albums failed for {section_key} ({channel_id}): {exc}", file=sys.stderr)
                                results.extend(section.get("results") or [])
                        else:
                            results.extend(section.get("results") or [])
                    self._send_json(200, json.dumps(results).encode())
                except Exception as exc:
                    self._send_error(500, str(exc))

            else:
                self._send_error(404, f"unknown sub-path: {parts[1]}")
        elif path == "/search":
            q = (query.get("q") or [""])[0].strip()
            if not q:
                self._send_error(400, "missing q")
                return
            try:
                limit = max(1, min(20, int((query.get("limit") or ["8"])[0])))
            except ValueError:
                limit = 8
            try:
                results = []
                for kind in ("songs", "videos"):
                    for r in yt.search(q, filter=kind, limit=limit)[:limit]:
                        if not r.get("videoId"):
                            continue
                        album = r.get("album") or {}
                        thumbs = r.get("thumbnails") or []
                        results.append({
                            "video_id": r["videoId"],
                            "kind": kind[:-1],
                            "title": r.get("title"),
                            "artists": [a.get("name") for a in r.get("artists") or [] if a.get("name")],
                            "album": album.get("name"),
                            "duration_seconds": r.get("duration_seconds"),
                            "thumbnail": thumbs[0]["url"] if thumbs else None,
                        })
                self._send_json(200, json.dumps(results).encode())
            except Exception as exc:
                self._send_error(500, str(exc))
        elif path.startswith("/albums/"):
            browse_id = urllib.parse.unquote(path[len("/albums/"):])
            if not browse_id:
                self._send_error(400, "missing browseId")
                return
            try:
                data = yt.get_album(browse_id)
                self._send_json(200, json.dumps(data).encode())
            except Exception as exc:
                self._send_error(500, str(exc))

        else:
            self._send_error(404, f"unknown path: {path}")

    def _send_json(self, status: int, body: bytes):
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _send_error(self, status: int, message: str):
        body = json.dumps({"error": message}).encode()
        self._send_json(status, body)


if __name__ == "__main__":
    host = os.environ.get("HOST", "0.0.0.0")
    port = int(os.environ.get("PORT", 9001))
    server = HTTPServer((host, port), Handler)
    print(f"[ytmusicapi_server] listening on http://{host}:{port}", file=sys.stderr)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
