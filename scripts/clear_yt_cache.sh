#!/usr/bin/env bash
# Delete all YouTube Data API entries from the HTTP cache DB.
# Usage: ./clear_yt_cache.sh [path/to/http_cache.db]
set -euo pipefail

CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/musiclib-rs"
DB="${1:-$CACHE_DIR/http_cache.db}"

if [[ ! -f "$DB" ]]; then
    echo "error: $DB not found" >&2
    exit 1
fi

before=$(sqlite3 "$DB" "SELECT COUNT(*) FROM cache_entry WHERE key LIKE '%googleapis.com/youtube/v3/%';")
total=$(sqlite3 "$DB" "SELECT COUNT(*) FROM cache_entry;")
echo "$DB: $before YouTube entries out of $total total"

if [[ "$before" == "0" ]]; then
    echo "nothing to delete"
    exit 0
fi

sqlite3 "$DB" <<SQL
DELETE FROM cache_entry WHERE key LIKE '%googleapis.com/youtube/v3/%';
VACUUM;
SQL

after_total=$(sqlite3 "$DB" "SELECT COUNT(*) FROM cache_entry;")
echo "deleted $before; $after_total rows remain"
