#!/usr/bin/env python3
"""Register the roots in <config_dir>/update_roots/NAME.txt with the player.

One-off move from update.sh's roots files to the player's roots
(docs/plan-update-roots.md). Each URL in NAME.txt becomes a root with fetch
options NAME (entry point main). `#` lines are comments. A URL the player
already has a root for is reported and skipped.

Usage: scripts/import_update_roots.py [--server http://127.0.0.1:4600] [ROOTS_DIR]
"""

import argparse
import json
import os
import pathlib
import urllib.error
import urllib.request


def main():
    config = pathlib.Path(os.environ.get("XDG_CONFIG_HOME", pathlib.Path.home() / ".config"))
    ap = argparse.ArgumentParser()
    ap.add_argument("--server", default="http://127.0.0.1:4600")
    ap.add_argument("roots_dir", nargs="?", default=config / "musiclib-rs" / "update_roots")
    args = ap.parse_args()

    added = skipped = 0
    for path in sorted(pathlib.Path(args.roots_dir).glob("*.txt")):
        for line in path.read_text().splitlines():
            url = line.strip()
            if not url or url.startswith("#"):
                continue
            body = json.dumps({"url": url, "fetch_options": {"file": path.stem}}).encode()
            req = urllib.request.Request(
                f"{args.server}/api/roots",
                data=body,
                headers={"Content-Type": "application/json"},
                method="POST",
            )
            try:
                with urllib.request.urlopen(req) as res:
                    root = json.load(res)
                print(f"added {root['id']}: {url} ({path.stem})")
                added += 1
            except urllib.error.HTTPError as e:
                print(f"skipped {url}: {json.load(e).get('message')}")
                skipped += 1
    print(f"{added} added, {skipped} skipped")


if __name__ == "__main__":
    main()
