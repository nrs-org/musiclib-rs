#!/usr/bin/env -S uv run
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     "ytmusicapi>=1.7.0",
# ]
# ///
"""Minimal HTTP server exposing ytmusicapi artist data.

Endpoint:
  GET /artists/<channel_id>
    Calls ytmusicapi.YTMusic().get_artist(channel_id) and returns the JSON response.
    <channel_id> must be a YouTube channel ID starting with "UC".

Usage:
  PORT=9001 uv run src/bin/ytmusicapi_server.py

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
                    artist = yt.get_artist(channel_id)
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
    port = int(os.environ.get("PORT", 9001))
    server = HTTPServer(("0.0.0.0", port), Handler)
    print(f"[ytmusicapi_server] listening on http://0.0.0.0:{port}", file=sys.stderr)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
