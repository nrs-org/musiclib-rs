#!/usr/bin/env bash
# Daily library update, meant for cron (see docs/plan-update.md).
#
# Each file <config_dir>/update_roots/NAME.txt lists root URLs (one per line,
# `#` comments allowed) imported with fetch options NAME.yaml from
# <config_dir>/fetch_options/. Every run refreshes the growing listings its
# roots reach; the last one then spends the rest of the day's YouTube budget
# on full refreshes of the oldest uploads snapshots.
#
# First, the MusicBrainz URL mirror is caught up to the MusicBrainz server,
# never past it, and the update stops if a self-hosted server's own
# replication has fallen behind.
#
# Environment:
#   QUOTA_BUDGET    YouTube units the update may use per day (default 8000),
#                   leaving the rest of the 10k for manual refreshes.
#   ROOTS_DIR       roots directory (default <config_dir>/update_roots).
#   IMPORT          import command (default: cargo run --release --bin import --).
#   MB_SYNC         mirror sync command
#                   (default: cargo run --release --bin mb_sync_replication --).
#   MB_MAX_LAG      packets (about one an hour) a self-hosted MusicBrainz
#                   server may be behind the newest (default 36: it
#                   replicates daily).
#   MB_ALLOW_STALE  set to skip that check (the mirror still stops at the
#                   server).
set -euo pipefail

config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/musiclib-rs"
roots_dir="${ROOTS_DIR:-$config_dir/update_roots}"
budget="${QUOTA_BUDGET:-8000}"
read -r -a import <<<"${IMPORT:-cargo run --release --bin import --}"
read -r -a mb_sync <<<"${MB_SYNC:-cargo run --release --bin mb_sync_replication --}"

shopt -s nullglob
roots=("$roots_dir"/*.txt)
if ((${#roots[@]} == 0)); then
  echo "no roots files in $roots_dir" >&2
  exit 1
fi

export RUST_LOG="${RUST_LOG:-musiclib_rs=info,import=info,mb_sync_replication=info}"

echo "== MusicBrainz mirror" >&2
lag=()
[[ -n "${MB_ALLOW_STALE:-}" ]] || lag=(--max-server-lag "${MB_MAX_LAG:-36}")
if ! "${mb_sync[@]}" "${lag[@]}"; then
  echo "MusicBrainz mirror/server check failed; not updating" >&2
  exit 1
fi
status=0
for i in "${!roots[@]}"; do
  file="${roots[$i]}"
  name="$(basename "$file" .txt)"
  extra=()
  if ((i < ${#roots[@]} - 1)); then
    extra+=(--no-full-refresh-pass)
  fi
  echo "== $name" >&2
  # One failing roots file shouldn't stop the others.
  "${import[@]}" --url-file "$file" --fetch-options "$name.yaml" \
    --refresh --quota-budget "$budget" "${extra[@]}" "$@" || status=$?
done
exit "$status"
